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
  const NETWORK_GRACE_MS = 8000;
  const CHANNEL_OPEN_MS = 15000;
  const MAX_TRANSCRIPT_ITEMS = 200;

  // One object per connection. Anything arriving for an older one is ignored.
  let session = null;

  const say = (text) => {
    status.textContent = text;
  };

  function apiHeaders(extra) {
    return { "Content-Type": "application/json", "X-Nexus-Csrf": root.dataset.csrf, ...extra };
  }

  function showLive(live) {
    liveBox.hidden = !live;
    stopButton.hidden = !live;
    remaining.hidden = !live;
    connectButton.hidden = live;
    connectButton.disabled = false;
    holdButton.disabled = false;
    holdButton.classList.remove("holding");
    holdButton.setAttribute("aria-pressed", "false");
    handsFreeBox.checked = false;
  }

  // Every way out comes through here: the microphone must never outlive the session.
  function stop(message) {
    const ending = session;
    if (!ending) return;
    session = null;
    clearInterval(ending.ticker);
    clearTimeout(ending.channelTimer);
    clearTimeout(ending.graceTimer);
    if (ending.channel) ending.channel.close();
    if (ending.peer) ending.peer.close();
    if (ending.microphone) {
      for (const track of ending.microphone.getTracks()) track.stop();
    }
    if (ending.audio) ending.audio.srcObject = null;
    showLive(false);
    say(message);
  }

  const send = (live, event) => {
    if (live.channel && live.channel.readyState === "open") {
      live.channel.send(JSON.stringify(event));
    }
  };

  function touch(live) {
    live.lastActivity = performance.now();
  }

  function setMic(live, on) {
    if (live.track) live.track.enabled = on;
  }

  function press() {
    const live = session;
    if (!live || !live.ready || live.handsFree || live.holding) return;
    live.holding = true;
    touch(live);
    holdButton.classList.add("holding");
    holdButton.setAttribute("aria-pressed", "true");
    holdButton.textContent = "Listening… release to send";
    send(live, { type: "input_audio_buffer.clear" });
    if (live.responding) send(live, { type: "response.cancel" });
    if (live.audioPlaying) send(live, { type: "output_audio_buffer.clear" });
    setMic(live, true);
  }

  function release() {
    const live = session;
    if (!live || !live.holding) return;
    live.holding = false;
    touch(live);
    setMic(live, false);
    holdButton.classList.remove("holding");
    holdButton.setAttribute("aria-pressed", "false");
    holdButton.textContent = "Hold to talk";
    send(live, { type: "input_audio_buffer.commit" });
    send(live, { type: "response.create" });
  }

  function setHandsFree(on) {
    const live = session;
    if (!live || !live.ready) return;
    if (on) release();
    live.handsFree = on;
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

  function transcriptItem(live, key, who) {
    let item = live.items.get(key);
    if (!item) {
      item = document.createElement("li");
      item.className = who;
      transcript.append(item);
      live.items.set(key, item);
      while (transcript.children.length > MAX_TRANSCRIPT_ITEMS) transcript.firstElementChild.remove();
    }
    item.scrollIntoView({ block: "nearest" });
    return item;
  }

  function addTool(name) {
    const item = document.createElement("li");
    const label = document.createElement("span");
    const state = document.createElement("span");
    label.textContent = name;
    state.className = "tag";
    state.textContent = "running";
    item.append(label, " ", state);
    toolList.append(item);
    return state;
  }

  // The tool's answer as the model should read it, and what the list says about it.
  async function runTool(live, call) {
    let args;
    try {
      args = JSON.parse(call.arguments || "{}");
    } catch {
      const error = { error: { code: "voice_invalid_arguments" } };
      return { output: JSON.stringify(error), label: "refused: voice_invalid_arguments" };
    }
    let response;
    try {
      response = await fetch("/api/voice/tool", {
        method: "POST",
        headers: apiHeaders({ "X-Nexus-Voice-Session": live.id }),
        body: JSON.stringify({ name: call.name, arguments: args }),
      });
    } catch {
      const error = { error: { code: "voice_network_error" } };
      return { output: JSON.stringify(error), label: "failed: network" };
    }
    let body = {};
    try {
      body = await response.json();
    } catch {
      body = { error: { code: `voice_http_${response.status}` } };
    }
    if (body.error) {
      const expired = body.error.code === "voice_session_invalid";
      const { code } = body.error;
      return { output: JSON.stringify({ error: { code } }), label: `refused: ${code}`, expired };
    }
    let label = body.isError ? "refused" : "done";
    if (body.isError) {
      try {
        const { error } = JSON.parse(body.output);
        const tokens = (error.violations || []).map((violation) => violation.token).join(", ");
        label = `refused: ${error.code}${tokens ? ` (${tokens})` : ""}`;
      } catch {
        label = "failed";
      }
    }
    return { output: body.output, label };
  }

  // All of a response's calls get answered, in order, then one `response.create` follows.
  async function answerCalls(live, calls) {
    for (const call of calls) {
      touch(live);
      const state = addTool(call.name);
      const answer = await runTool(live, call);
      if (session !== live) return;
      state.textContent = answer.label;
      send(live, {
        type: "conversation.item.create",
        item: { type: "function_call_output", call_id: call.call_id, output: answer.output },
      });
      if (answer.expired) {
        stop("The session has expired. Connect again to go on.");
        return;
      }
    }
    touch(live);
    // Someone holding the button will ask for the response when they let go.
    if (!live.holding) send(live, { type: "response.create" });
  }

  function onEvent(live, event) {
    switch (event.type) {
      case "input_audio_buffer.speech_started":
        touch(live);
        break;
      case "conversation.item.input_audio_transcription.delta":
        touch(live);
        transcriptItem(live, event.item_id, "you").textContent += event.delta || "";
        break;
      case "conversation.item.input_audio_transcription.completed":
        touch(live);
        transcriptItem(live, event.item_id, "you").textContent = event.transcript || "";
        break;
      case "response.output_audio_transcript.delta":
      case "response.output_text.delta":
        touch(live);
        transcriptItem(live, event.item_id, "model").textContent += event.delta || "";
        break;
      case "response.output_audio_transcript.done":
        touch(live);
        transcriptItem(live, event.item_id, "model").textContent = event.transcript || "";
        break;
      case "response.output_text.done":
        touch(live);
        transcriptItem(live, event.item_id, "model").textContent = event.text || "";
        break;
      case "output_audio_buffer.started":
        live.audioPlaying = true;
        break;
      case "output_audio_buffer.stopped":
      case "output_audio_buffer.cleared":
        live.audioPlaying = false;
        break;
      case "response.created":
        live.responding = true;
        break;
      case "response.done": {
        live.responding = false;
        touch(live);
        const output = (event.response && event.response.output) || [];
        const calls = output.filter((item) => item.type === "function_call");
        if (calls.length) answerCalls(live, calls);
        break;
      }
      case "error":
        // A press too short to hold audio, or a cancel that came late, is nothing to report.
        if (
          !["input_audio_buffer_commit_empty", "response_cancel_not_active"].includes(
            event.error && event.error.code,
          )
        ) {
          say("The voice service reported a problem. If it keeps happening, stop and connect again.");
        }
        break;
      default:
    }
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

  function networkLost(live) {
    if (session === live) stop("The connection was lost, and the microphone is off. Check the network and connect again.");
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
    const live = { items: new Map(), lastActivity: performance.now() };
    session = live;
    connectButton.disabled = true;
    say("Asking for the microphone…");
    try {
      live.microphone = await navigator.mediaDevices.getUserMedia({ audio: true });
    } catch (error) {
      const denied = error && error.name === "NotAllowedError";
      stop(
        denied
          ? "The microphone was refused. Allow it for this site in the browser's settings, then connect again."
          : "No microphone could be opened on this device.",
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
    try {
      const response = await fetch("/api/voice/session", {
        method: "POST",
        headers: apiHeaders(),
        body: JSON.stringify({ variant: root.dataset.variant }),
      });
      if (!response.ok) {
        stop(await mintFailure(response));
        return;
      }
      minted = await response.json();
    } catch {
      stop("The server could not be reached. Check the network and connect again.");
      return;
    }
    if (session !== live) return;
    live.id = minted.voiceSessionId;
    live.lifetimeMs = minted.lifetimeSeconds * 1000;
    live.idleMs = minted.idleSeconds * 1000;
    live.startedAt = performance.now();
    live.ticker = setInterval(() => tick(live), 1000);
    tick(live);

    try {
      const peer = new RTCPeerConnection();
      live.peer = peer;
      peer.addTrack(live.track, live.microphone);
      peer.addEventListener("track", (event) => {
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
        say("Connected. Hold the button and speak.");
      });
      channel.addEventListener("close", () => networkLost(live));
      live.channelTimer = setTimeout(() => networkLost(live), CHANNEL_OPEN_MS);

      await peer.setLocalDescription(await peer.createOffer());
      const answer = await fetch(minted.callsUrl, {
        method: "POST",
        headers: { Authorization: `Bearer ${minted.clientSecret}`, "Content-Type": "application/sdp" },
        body: peer.localDescription.sdp,
      });
      if (!answer.ok) {
        stop(`OpenAI refused the connection (${answer.status}). Connect again to retry.`);
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
  stopButton.addEventListener("click", () => stop("Stopped. The microphone is off."));
  handsFreeBox.addEventListener("change", () => setHandsFree(handsFreeBox.checked));

  holdButton.addEventListener("pointerdown", (event) => {
    if (event.button !== 0) return;
    event.preventDefault();
    press();
  });
  holdButton.addEventListener("pointerup", release);
  holdButton.addEventListener("pointercancel", release);
  holdButton.addEventListener("pointerleave", release);
  holdButton.addEventListener("contextmenu", (event) => event.preventDefault());

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
  window.addEventListener("blur", release);
  document.addEventListener("visibilitychange", () => {
    if (!session) return;
    if (document.hidden) {
      release();
      setMic(session, false);
    } else if (session.handsFree) {
      setMic(session, true);
    }
  });
  window.addEventListener("pagehide", () => stop("Stopped."));
  document.addEventListener("freeze", () => stop("Stopped."));
})();
