// Passkey login and registration on the Nexus owner pages. No library: the server's WebAuthn JSON
// carries binary fields as base64url, which the browser API takes as ArrayBuffers.
"use strict";
(() => {
  const decode = (text) => {
    const base64 = text.replace(/-/g, "+").replace(/_/g, "/");
    const binary = atob(base64.padEnd(Math.ceil(base64.length / 4) * 4, "="));
    return Uint8Array.from(binary, (c) => c.charCodeAt(0)).buffer;
  };
  const encode = (buffer) =>
    btoa(String.fromCharCode(...new Uint8Array(buffer)))
      .replace(/\+/g, "-")
      .replace(/\//g, "_")
      .replace(/=+$/, "");

  const post = async (path, body) => {
    const response = await fetch(path, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body ?? {}),
      credentials: "same-origin",
    });
    const data = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error(data.message || `The server answered ${response.status}.`);
    return data;
  };

  const say = (text) => {
    document.getElementById("passkey-status").textContent = text;
  };
  const why = (error) =>
    error && error.name === "NotAllowedError"
      ? "The passkey was not used. Try again, or use the passphrase."
      : (error && error.message) || "That did not work.";

  async function logIn() {
    const { publicKey } = await post("/login/passkey/start");
    publicKey.challenge = decode(publicKey.challenge);
    for (const allowed of publicKey.allowCredentials || []) allowed.id = decode(allowed.id);
    const credential = await navigator.credentials.get({ publicKey });
    const form = document.querySelector('form[action="/login"]');
    const reply = credential.response;
    const done = await post("/login/passkey/finish", {
      remember: form?.querySelector('input[name="remember"]')?.checked ?? false,
      next: form?.querySelector('input[name="next"]')?.value ?? "",
      credential: {
        id: credential.id,
        rawId: encode(credential.rawId),
        type: credential.type,
        extensions: credential.getClientExtensionResults(),
        response: {
          authenticatorData: encode(reply.authenticatorData),
          clientDataJSON: encode(reply.clientDataJSON),
          signature: encode(reply.signature),
          userHandle: reply.userHandle ? encode(reply.userHandle) : null,
        },
      },
    });
    location.assign(done.redirect);
  }

  async function add(box) {
    const csrf = box.dataset.csrf;
    const { publicKey } = await post("/login/passkeys/register/start", {
      csrf,
      name: document.getElementById("passkey-name").value,
      passphrase: document.getElementById("passkey-passphrase").value,
    });
    publicKey.challenge = decode(publicKey.challenge);
    publicKey.user.id = decode(publicKey.user.id);
    for (const excluded of publicKey.excludeCredentials || []) excluded.id = decode(excluded.id);
    const credential = await navigator.credentials.create({ publicKey });
    const reply = credential.response;
    await post("/login/passkeys/register/finish", {
      csrf,
      credential: {
        id: credential.id,
        rawId: encode(credential.rawId),
        type: credential.type,
        extensions: credential.getClientExtensionResults(),
        response: {
          attestationObject: encode(reply.attestationObject),
          clientDataJSON: encode(reply.clientDataJSON),
          transports: reply.getTransports ? reply.getTransports() : undefined,
        },
      },
    });
    location.reload();
  }

  for (const [id, action, waiting] of [
    ["passkey-login", logIn, "Waiting for your passkey…"],
    ["passkey-add", add, "Follow your device's prompt…"],
  ]) {
    const box = document.getElementById(id);
    if (!box) continue;
    box.hidden = false;
    const button = box.querySelector("button");
    if (!window.PublicKeyCredential) {
      button.disabled = true;
      say("This browser can't use passkeys.");
      continue;
    }
    // A passkey belongs to the host it was made on.
    if (box.dataset.origin !== location.origin) {
      button.disabled = true;
      say(`Passkeys work only at ${box.dataset.origin}.`);
      continue;
    }
    button.addEventListener("click", async () => {
      button.disabled = true;
      say(waiting);
      try {
        await action(box);
      } catch (error) {
        say(why(error));
        button.disabled = false;
      }
    });
  }
})();
