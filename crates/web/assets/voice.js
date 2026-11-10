// The voice session: OpenAI Realtime over WebRTC, from this page. The server mints the secret and
// runs every tool call behind its allowlist; this file only relays. Text from the model is shown
// with textContent and nowhere as markup, because a job description can steer what it says.
(() => {
  const root = document.getElementById("voice");
  if (!root || !root.dataset.csrf) return;

  const byId = (id) => document.getElementById(id);
  const status = byId("voice-status");
  const connectButton = byId("voice-connect");
  const stopButton = byId("voice-stop");
  const liveBox = byId("voice-live");
  const holdButton = byId("voice-hold");
  const handsFreeBox = byId("voice-handsfree");
  const remaining = byId("voice-remaining");
  const transcript = byId("voice-transcript");
  const toolList = byId("voice-tools");
  const phaseLabel = byId("voice-phase");
  const micLabel = byId("voice-mic-state");
  const holdLabel = byId("voice-hold-label") || holdButton;
  const interruptButton = byId("voice-interrupt");
  const announcement = byId("voice-announcement");
  const TOOL_LABELS = {
    list_applications: "Find applications", list_applications_filtered: "Find applications",
    get_application: "Read application", find_application_by_job_url: "Find application",
    create_application: "Create application", update_application: "Update application",
    archive_application: "Archive application", unarchive_application: "Restore application",
    append_application_note: "Add note", record_application_event: "Update application history",
    list_application_events: "Read history", list_application_activity: "Read activity",
    create_contact: "Create contact", update_contact: "Update contact",
    list_documents: "Find documents", get_document: "Read document details",
    update_document_links: "Link documents", update_document_metadata: "Update document details",
    get_cv_source: "Read CV", patch_cv: "Edit CV draft", render_cv_pdf: "Preview CV",
    list_cv_revisions: "Read CV history", revert_cv: "Restore CV revision",
  };
  const PHASES = {
    idle: ["Ready when you are", "Start a conversation to use your microphone."],
    connecting: ["Connecting", "Allow microphone access when your browser asks."],
    ready: ["Your turn", "Tap the microphone to talk, then tap again to send."],
    listening: ["Listening", "Tap again to send your message."],
    thinking: ["Thinking", "You can interrupt at any time."],
    speaking: ["Speaking", "Interrupt to stop the reply."],
    working: ["Working", "Checking your request in Nexus."],
    paused: ["Microphone paused", "Return to this window to continue."],
    error: ["Needs attention", "Check the message below and try again."],
    ended: ["Conversation ended", "Your conversation stays here until you start a new one."],
  };
  const NETWORK_GRACE_MS = 8000;
  const CHANNEL_OPEN_MS = 15000;
  const MAX_TRANSCRIPT_ITEMS = 200;
  const CHECK_MS = 20000;
  const CHECK_TIMEOUT_MS = 10000;
  const FAILED_CHECKS_TO_END = 2;
  const WRITE_TOOLS = new Set([
    "create_application",
    "update_application",
    "archive_application",
    "unarchive_application",
    "append_application_note",
    "record_application_event",
    "create_contact",
    "update_contact",
    "update_document_links",
    "update_document_metadata",
    "patch_cv",
    "revert_cv",
  ]);
  // Shorter than this holds too little sound for OpenAI to accept, and asks for nothing.
  const MIN_HOLD_MS = 300;

  // One object per connection. Anything arriving for an older one is ignored.
  let session = null;
  root.dataset.sessionState = "stopped";

  const say = (text) => {
    status.textContent = text;
  };

  function apiHeaders(extra) {
    return { "Content-Type": "application/json", "X-Nexus-Csrf": root.dataset.csrf, ...extra };
  }

  function setPhase(phase, message) {
    const changed = root.dataset.phase !== phase;
    root.dataset.phase = phase;
    if (phaseLabel) phaseLabel.textContent = PHASES[phase][0];
    if (message || changed) say(message || PHASES[phase][1]);
  }

  function syncState(live = session) {
    const micOn = !!(live?.track?.enabled && !document.hidden);
    if (micLabel) micLabel.textContent = micOn ? "Microphone on" : "Microphone off";
    root.dataset.mic = micOn ? "on" : "off";
    if (!live || session !== live) return;
    const phase = !live.ready ? "connecting" : live.error ? "error"
      : (live.holding || live.inputSpeaking) && micOn ? "listening" : live.audioPlaying ? "speaking"
      : live.toolRunning ? "working" : live.responding || live.awaiting ? "thinking"
      : document.hidden ? "paused" : live.handsFree && micOn ? "listening" : "ready";
    setPhase(phase);
    if (phase === "listening" && live.handsFree) say("Speak naturally. Turn off hands-free to mute.");
    holdButton.disabled = !live.ready || !!live.handsFree;
    holdLabel.textContent = live.holding ? "Send voice" : "Tap to talk";
    holdButton.setAttribute("aria-pressed", String(!!live.holding));
    if (interruptButton) interruptButton.disabled = !(live.responding || live.audioPlaying || live.awaiting || live.toolRunning);
  }

  function showLive(active) {
    liveBox.hidden = !active;
    stopButton.hidden = !active;
    remaining.hidden = !session?.startedAt;
    connectButton.hidden = active;
    connectButton.disabled = false;
    holdButton.classList.remove("holding");
    handsFreeBox.checked = !!session?.handsFree;
    syncState();
  }

  // Every way out comes through here: the microphone must never outlive the session.
  function stop(message, error = false) {
    const ending = session;
    if (!ending) return;
    session = null;
    root.dataset.sessionState = "stopped";
    clearInterval(ending.ticker);
    clearInterval(ending.checker);
    clearTimeout(ending.channelTimer);
    clearTimeout(ending.graceTimer);
    if (ending.toolAbort) ending.toolAbort.abort();
    if (ending.connectAbort) ending.connectAbort.abort();
    if (ending.toolState) {
      ending.toolState.textContent = "Result unknown — check the record";
      ending.toolState.dataset.state = "unknown";
    }
    if (ending.channel) ending.channel.close();
    if (ending.peer) ending.peer.close();
    if (ending.microphone) {
      for (const track of ending.microphone.getTracks()) track.stop();
    }
    if (ending.audio) ending.audio.srcObject = null;
    showLive(false);
    setPhase(error ? "error" : "ended", message);
  }

  const send = (live, event) => {
    if (live.channel && live.channel.readyState === "open") {
      if (event.type === "response.create") {
        live.responsePending = true;
        live.acceptNextResponse = true;
      }
      live.channel.send(JSON.stringify(event));
    }
  };

  function touch(live) {
    live.lastActivity = performance.now();
  }

  function setMic(live, on) {
    if (live.track) live.track.enabled = on;
    syncState(live);
  }

  function interrupt(live = session) {
    if (!live || !live.ready) return;
    live.generation += 1;
    if (live.responsePending) live.discardCreated += 1;
    live.responsePending = false;
    live.acceptNextResponse = false;
    live.blockResponse = true;
    if (live.responseId) live.cancelledResponses.add(live.responseId);
    if (live.responding || live.awaiting) send(live, { type: "response.cancel" });
    if (live.audioPlaying) send(live, { type: "output_audio_buffer.clear" });
    live.responding = false;
    live.awaiting = false;
    live.audioPlaying = false;
    live.responseOwed = false;
    live.error = false;
    touch(live);
    syncState(live);
    say(live.toolRunning ? "Reply interrupted. An action already started may still finish." : "Reply interrupted. You can speak again.");
  }

  function press() {
    const live = session;
    if (!live || !live.ready || live.handsFree || live.holding || document.hidden) return;
    if (live.responding || live.audioPlaying || live.awaiting || live.toolRunning) interrupt(live);
    live.error = false;
    live.holding = true;
    live.pressedAt = performance.now();
    touch(live);
    holdButton.classList.add("holding");
    send(live, { type: "input_audio_buffer.clear" });
    setMic(live, true);
  }

  function release() {
    const live = session;
    if (!live || !live.holding) return;
    live.holding = false;
    touch(live);
    holdButton.classList.remove("holding");
    if (performance.now() - live.pressedAt < MIN_HOLD_MS) {
      send(live, { type: "input_audio_buffer.clear" });
      if (live.responseOwed) {
        live.awaiting = true;
        send(live, { type: "response.create" });
      }
    } else {
      live.awaiting = true;
      send(live, { type: "input_audio_buffer.commit" });
      send(live, { type: "response.create" });
    }
    live.responseOwed = false;
    setMic(live, false);
  }

  function setHandsFree(on) {
    const live = session;
    if (!live || !live.ready) return;
    if (on) release();
    live.handsFree = on;
    live.error = false;
    if (!on) live.inputSpeaking = false;
    touch(live);
    holdButton.disabled = on;
    send(live, {
      type: "session.update",
      session: {
        type: "realtime",
        audio: { input: { turn_detection: on ? { type: "semantic_vad" } : null } },
      },
    });
    setMic(live, on && !document.hidden);
    if (!on) send(live, { type: "input_audio_buffer.clear" });
  }

  function writeTranscript(live, key, who, text, append = false, final = false) {
    const atBottom = transcript.scrollHeight - transcript.scrollTop - transcript.clientHeight < 60;
    let item = live.items.get(key);
    if (!item) {
      const row = document.createElement("li");
      row.className = who;
      const speaker = document.createElement("span");
      speaker.className = "speaker";
      speaker.textContent = who === "you" ? "You" : "Assistant";
      item = document.createElement("span");
      item.className = "message-text";
      row.append(speaker, item);
      transcript.append(row);
      live.items.set(key, item);
      while (transcript.children.length > MAX_TRANSCRIPT_ITEMS) transcript.firstElementChild.remove();
    }
    item.textContent = append ? item.textContent + text : text;
    if (byId("voice-transcript-empty")) byId("voice-transcript-empty").hidden = true;
    if (atBottom) transcript.scrollTop = transcript.scrollHeight;
    if (final && announcement) announcement.textContent = `${who === "you" ? "You" : "Assistant"}: ${item.textContent}`;
  }

  function addTool(name) {
    const item = document.createElement("li");
    const label = document.createElement("span");
    const state = document.createElement("span");
    label.textContent = TOOL_LABELS[name] || "Requested action";
    state.className = "tag";
    state.dataset.state = "running";
    state.textContent = "In progress";
    item.append(label, " ", state);
    toolList.append(item);
    if (byId("voice-tools-empty")) byId("voice-tools-empty").hidden = true;
    return state;
  }

  // What to tell the person if this answer from the server means their session is over.
  function endedBy(status, code) {
    if (status === 401) {
      return "You are logged out, or the server restarted, so this session has ended and the microphone is off. Log in again to go on.";
    }
    if (code === "voice_session_invalid" || code === "csrf_invalid") {
      return "The server ended this session, and the microphone is off. Connect again to go on.";
    }
    return null;
  }

  // The tool's answer as the model should read it, and what the list says about it.
  async function runTool(live, call) {
    if (session !== live) return { output: "{}", label: "stopped" };
    let args;
    try {
      args = JSON.parse(call.arguments || "{}");
    } catch {
      const error = { error: { code: "voice_invalid_arguments" } };
      return { output: JSON.stringify(error), label: "Could not understand the action", state: "failed" };
    }
    let response;
    const controller = new AbortController();
    live.toolAbort = controller;
    try {
      response = await fetch("/api/voice/tool", {
        method: "POST",
        headers: apiHeaders({ "X-Nexus-Voice-Session": live.id }),
        body: JSON.stringify({ name: call.name, arguments: args }),
        signal: controller.signal,
      });
    } catch {
      const error = { error: { code: "voice_network_error" } };
      return { output: JSON.stringify(error), label: WRITE_TOOLS.has(call.name) ? "Result unknown — check the record" : "Connection failed", state: "unknown" };
    } finally {
      if (live.toolAbort === controller) live.toolAbort = null;
    }
    let body;
    try { body = await response.json(); } catch { body = null; }
    if (!body || typeof body !== "object" || response.status >= 500) {
      return {
        output: JSON.stringify({ error: { code: "voice_invalid_response" } }),
        label: WRITE_TOOLS.has(call.name) ? "Result unknown — check the record" : "Could not read the result",
        state: "unknown",
        ended: endedBy(response.status),
      };
    }
    if (body.error) {
      const code = body.error.code || `voice_http_${response.status}`;
      const ended = endedBy(response.status, code);
      return { output: JSON.stringify({ error: { code } }), label: "Action not completed", state: "failed", ended };
    }
    if (response.ok === false || typeof body.output !== "string") {
      return {
        output: JSON.stringify({ error: { code: "voice_invalid_response" } }),
        label: WRITE_TOOLS.has(call.name) ? "Result unknown — check the record" : "Could not read the result",
        state: "unknown",
        ended: endedBy(response.status),
      };
    }
    let dryRun = false;
    try { dryRun = JSON.parse(body.output).dryRun === true; } catch {}
    const changed = !body.isError && !dryRun && WRITE_TOOLS.has(call.name);
    return {
      output: body.output,
      label: body.isError ? "Not completed" : dryRun ? "Preview only" : changed ? "Saved" : "Done",
      state: body.isError ? "failed" : dryRun ? "preview" : changed ? "saved" : "done",
      updated: changed,
    };
  }

  function announceUpdate(tool) {
    const message = { type: "nexus-assistant-updated", tool };
    if (window.opener && !window.opener.closed) {
      window.opener.postMessage(message, window.location.origin);
    }
    if ("BroadcastChannel" in window) {
      const channel = new BroadcastChannel("nexus-assistant");
      channel.postMessage(message);
      channel.close();
    }
  }

  // All of a response's calls get answered, in order, then one `response.create` follows.
  async function answerCalls(live, calls, generation) {
    let answered = false;
    for (const call of calls) {
      if (session !== live || generation !== live.generation) return;
      // Repeated response.done events must not repeat a write. A failed network outcome is unknown,
      // so its call id stays consumed and is never retried automatically.
      if (!call.call_id || live.handledCalls.has(call.call_id)) continue;
      live.handledCalls.add(call.call_id);
      answered = true;
      touch(live);
      const state = addTool(call.name);
      live.toolRunning = true;
      live.toolState = state;
      syncState(live);
      const answer = await runTool(live, call);
      live.toolRunning = false;
      live.toolState = null;
      if (session !== live) return;
      state.textContent = answer.label;
      state.dataset.state = answer.state || "failed";
      if (answer.updated) announceUpdate(call.name);
      send(live, {
        type: "conversation.item.create",
        item: { type: "function_call_output", call_id: call.call_id, output: answer.output },
      });
      if (answer.ended) {
        stop(answer.ended, true);
        return;
      }
      if (generation !== live.generation) { syncState(live); return; }
    }
    if (!answered) return;
    touch(live);
    // Someone holding the button will ask for the response when they let go.
    if (live.holding) live.responseOwed = true;
    else { live.awaiting = true; send(live, { type: "response.create" }); }
    syncState(live);
  }

  function onEvent(live, event) {
    const responseId = event.response?.id || event.response_id;
    if (responseId && live.cancelledResponses.has(responseId)) return;
    switch (event.type) {
      case "input_audio_buffer.speech_started":
        live.inputSpeaking = true;
        live.error = false;
        touch(live);
        break;
      case "input_audio_buffer.speech_stopped":
        live.inputSpeaking = false;
        if (live.handsFree) {
          live.awaiting = true;
          live.responsePending = true;
          live.acceptNextResponse = true;
        }
        touch(live);
        break;
      case "conversation.item.input_audio_transcription.delta":
        touch(live);
        writeTranscript(live, event.item_id, "you", event.delta || "", true);
        break;
      case "conversation.item.input_audio_transcription.completed":
        touch(live);
        writeTranscript(live, event.item_id, "you", event.transcript || "", false, true);
        break;
      case "response.output_audio_transcript.delta":
      case "response.output_text.delta":
        touch(live);
        writeTranscript(live, event.item_id, "model", event.delta || "", true);
        break;
      case "response.output_audio_transcript.done":
        touch(live);
        writeTranscript(live, event.item_id, "model", event.transcript || "", false, true);
        break;
      case "response.output_text.done":
        touch(live);
        writeTranscript(live, event.item_id, "model", event.text || "", false, true);
        break;
      case "output_audio_buffer.started":
        live.audioPlaying = true;
        break;
      case "output_audio_buffer.stopped":
      case "output_audio_buffer.cleared":
        live.audioPlaying = false;
        break;
      case "response.created":
        if (live.discardCreated > 0 || (live.blockResponse && !live.acceptNextResponse)) {
          if (live.discardCreated > 0) live.discardCreated -= 1;
          if (responseId) live.cancelledResponses.add(responseId);
          send(live, { type: "response.cancel", ...(responseId ? { response_id: responseId } : {}) });
          return;
        }
        live.responsePending = false;
        live.acceptNextResponse = false;
        live.responseId = responseId;
        live.blockResponse = false;
        live.error = false;
        live.responding = true;
        live.awaiting = false;
        break;
      case "response.done": {
        live.responding = false;
        live.awaiting = false;
        touch(live);
        if (["failed", "incomplete"].includes(event.response?.status)) {
          live.error = true;
          syncState(live);
          say("The reply could not finish. Try asking again.");
        }
        const output = (event.response && event.response.output) || [];
        const calls = output.filter((item) => item.type === "function_call");
        // A cancelled response is someone saying "no, don't", whatever it had got as far as.
        if (calls.length && event.response.status === "completed" && !live.blockResponse) {
          const generation = live.generation;
          live.callQueue = live.callQueue.then(() => answerCalls(live, calls, generation));
        }
        break;
      }
      case "error":
        // A press too short to hold audio, or a cancel that came late, is nothing to report.
        if (
          !["input_audio_buffer_commit_empty", "response_cancel_not_active"].includes(
            event.error && event.error.code,
          )
        ) {
          live.error = true;
          syncState(live);
          say("Voice could not complete that request. Try speaking again, or end and reconnect.");
        }
        break;
      default:
    }
    syncState(live);
  }

  function tick(live) {
    if (session !== live) return;
    const left = live.lifetimeMs - (performance.now() - live.startedAt);
    if (left <= 0) {
      stop("The session reached its time limit, and the microphone is off. Connect again to go on.");
      return;
    }
    if (performance.now() - live.lastActivity >= live.idleMs) {
      stop("Disconnected after 5 quiet minutes, and the microphone is off. Connect again to go on.");
      return;
    }
    const seconds = Math.ceil(left / 1000);
    remaining.textContent = `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, "0")} left`;
  }

  // Only this page can close the connection to OpenAI, and hands-free can send sound for up to an
  // hour with no tool call to find out that the login has gone. A failed check proves nothing alone,
  // but a server that stays out of reach can't enforce anything either, so two in a row end it.
  async function checkStatus(live) {
    let status;
    let code;
    try {
      const response = await fetch("/api/voice/status", {
        headers: { "X-Nexus-Voice-Session": live.id },
        signal: AbortSignal.timeout(CHECK_TIMEOUT_MS),
      });
      ({ status } = response);
      if (!response.ok) ({ code } = (await response.json()).error);
    } catch {
      // Counted below, unless the status has already said enough.
    }
    if (session !== live) return;
    const ended = endedBy(status, code);
    if (ended) {
      stop(ended);
    } else if (status === 200) {
      live.failedChecks = 0;
    } else {
      live.failedChecks += 1;
      if (live.failedChecks >= FAILED_CHECKS_TO_END) {
        stop("The server can't be reached, so this session has ended and the microphone is off. Check the network and connect again.");
      }
    }
  }

  function networkLost(live) {
    if (session === live) stop("Connection lost. Check your network and start again.", true);
  }

  async function mintFailure(response) {
    let code = "";
    let message = "";
    try {
      ({ code, message } = (await response.json()).error);
    } catch {
      // A proxy's page, not ours: the generic text below covers it.
    }
    if (code === "voice_disabled") return "Voice is off: no OpenAI key is configured on this server.";
    return message || "The server could not open a voice session. Try again in a moment.";
  }

  // Microphone first: a refusal then costs no session.
  async function connect() {
    if (session) return;
    const live = {
      generation: 0,
      discardCreated: 0,
      cancelledResponses: new Set(),
      items: new Map(),
      handledCalls: new Set(),
      callQueue: Promise.resolve(),
      lastActivity: performance.now(),
    };
    session = live;
    root.dataset.sessionState = "connecting";
    transcript.replaceChildren();
    toolList.replaceChildren();
    if (byId("voice-transcript-empty")) byId("voice-transcript-empty").hidden = false;
    if (byId("voice-tools-empty")) byId("voice-tools-empty").hidden = false;
    if (announcement) announcement.textContent = "";
    showLive(true);
    setPhase("connecting", "Allow microphone access when your browser asks.");
    try {
      live.microphone = await navigator.mediaDevices.getUserMedia({ audio: true });
    } catch (error) {
      if (session !== live) return;
      const denied = error && error.name === "NotAllowedError";
      stop(
        denied
          ? "The microphone was refused. Allow it for this site in the browser's settings, then connect again."
          : "No microphone could be opened on this device. Check that it is connected.",
        true,
      );
      return;
    }
    if (session !== live) {
      for (const track of live.microphone.getTracks()) track.stop();
      return;
    }
    [live.track] = live.microphone.getAudioTracks();
    setMic(live, false);
    say("Connecting…");

    let minted;
    live.connectAbort = new AbortController();
    try {
      const request = {};
      if (root.dataset.variant) request.variant = root.dataset.variant;
      if (root.dataset.applicationId) request.applicationId = root.dataset.applicationId;
      const response = await fetch("/api/voice/session", {
        method: "POST",
        headers: apiHeaders(),
        body: JSON.stringify(request),
        signal: live.connectAbort.signal,
      });
      if (session !== live) return;
      if (!response.ok) {
        const message = await mintFailure(response);
        if (session === live) stop(message, true);
        return;
      }
      minted = await response.json();
    } catch {
      if (session === live) stop("The server could not be reached. Check the network and start again.", true);
      return;
    }
    if (session !== live) return;
    live.id = minted.voiceSessionId;
    live.lifetimeMs = minted.lifetimeSeconds * 1000;
    live.idleMs = minted.idleSeconds * 1000;
    live.startedAt = performance.now();
    live.ticker = setInterval(() => tick(live), 1000);
    live.failedChecks = 0;
    live.checker = setInterval(() => checkStatus(live), CHECK_MS);
    tick(live);

    try {
      const peer = new RTCPeerConnection();
      live.peer = peer;
      peer.addTrack(live.track, live.microphone);
      peer.addEventListener("track", (event) => {
        if (session !== live) return;
        live.audio = live.audio || new Audio();
        live.audio.autoplay = true;
        [live.audio.srcObject] = event.streams;
      });
      peer.addEventListener("connectionstatechange", () => {
        clearTimeout(live.graceTimer);
        if (peer.connectionState === "failed") networkLost(live);
        if (peer.connectionState === "disconnected") {
          live.graceTimer = setTimeout(() => networkLost(live), NETWORK_GRACE_MS);
        }
      });
      const channel = peer.createDataChannel("oai-events");
      live.channel = channel;
      channel.addEventListener("message", (event) => {
        if (session !== live) return;
        try {
          onEvent(live, JSON.parse(event.data));
        } catch {
          // One unreadable event mustn't end the session.
        }
      });
      channel.addEventListener("open", () => {
        clearTimeout(live.channelTimer);
        if (session !== live) return;
        live.ready = true;
        root.dataset.sessionState = "live";
        syncState(live);
      });
      channel.addEventListener("close", () => networkLost(live));
      live.channelTimer = setTimeout(() => networkLost(live), CHANNEL_OPEN_MS);

      await peer.setLocalDescription(await peer.createOffer());
      const answer = await fetch(minted.callsUrl, {
        method: "POST",
        headers: { Authorization: `Bearer ${minted.clientSecret}`, "Content-Type": "application/sdp" },
        body: peer.localDescription.sdp,
        signal: live.connectAbort.signal,
      });
      if (session !== live) return;
      if (!answer.ok) {
        stop("Voice connection refused. Start again to retry.", true);
        return;
      }
      const sdp = await answer.text();
      if (session !== live) return;
      await peer.setRemoteDescription({ type: "answer", sdp });
    } catch {
      networkLost(live);
      return;
    }
    if (session !== live) return;
    showLive(true);
    tick(live);
  }

  connectButton.addEventListener("click", connect);
  stopButton.addEventListener("click", () => stop("Microphone off. Start a new conversation when you’re ready."));
  interruptButton?.addEventListener("click", () => interrupt());
  handsFreeBox.addEventListener("change", () => setHandsFree(handsFreeBox.checked));

  holdButton.addEventListener("click", () => session?.holding ? release() : press());

  // Space talks unless it already means something where the focus is.
  const spaceIsTaken = (target) =>
    target instanceof Element &&
    target !== holdButton &&
    target.closest("input, textarea, select, button, a, summary, [contenteditable]");
  document.addEventListener("keydown", (event) => {
    if (event.code !== "Space" || event.repeat || !session || spaceIsTaken(event.target)) return;
    event.preventDefault();
    press();
  });
  document.addEventListener("keyup", (event) => {
    if (event.code !== "Space" || !session?.holding) return;
    event.preventDefault();
    release();
  });

  // A phone must not keep sending sound from a pocket.
  window.addEventListener("blur", () => { release(); syncState(); });
  document.addEventListener("visibilitychange", () => {
    if (!session) return;
    if (document.hidden) {
      release();
      setMic(session, false);
    } else if (session.handsFree) {
      setMic(session, true);
    }
    syncState();
  });
  window.addEventListener("pagehide", () => stop("Stopped."));
  document.addEventListener("freeze", () => stop("Stopped."));
  syncState();
})();
