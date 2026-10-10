// Thin glue between the Rust UI and WebAuthn: the head speaks JSON with
// base64url strings; the browser API wants ArrayBuffers.
(() => {
  const dec = (s) => Uint8Array.from(atob(s.replace(/-/g, "+").replace(/_/g, "/")), (c) => c.charCodeAt(0));
  const enc = (buf) =>
    btoa(String.fromCharCode(...new Uint8Array(buf))).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");

  window.c6Passkey = {
    // Passkeys work here: the API exists and the page is a secure context.
    supported() {
      return !!window.PublicKeyCredential && window.isSecureContext;
    },
    // navigator.credentials.create; resolves to the JSON the head expects.
    async create(optionsJson) {
      const o = JSON.parse(optionsJson);
      o.challenge = dec(o.challenge);
      o.user.id = dec(o.user.id);
      for (const c of o.excludeCredentials || []) c.id = dec(c.id);
      const cred = await navigator.credentials.create({ publicKey: o });
      const r = cred.response;
      return JSON.stringify({
        id: cred.id,
        rawId: enc(cred.rawId),
        clientDataJson: enc(r.clientDataJSON),
        authenticatorData: enc(r.getAuthenticatorData()),
        publicKey: enc(r.getPublicKey()),
        publicKeyAlgorithm: r.getPublicKeyAlgorithm(),
      });
    },
    // navigator.credentials.get; resolves to the JSON the head expects.
    async get(optionsJson) {
      const o = JSON.parse(optionsJson);
      o.challenge = dec(o.challenge);
      for (const c of o.allowCredentials || []) c.id = dec(c.id);
      const cred = await navigator.credentials.get({ publicKey: o });
      const r = cred.response;
      return JSON.stringify({
        id: cred.id,
        rawId: enc(cred.rawId),
        clientDataJson: enc(r.clientDataJSON),
        authenticatorData: enc(r.authenticatorData),
        signature: enc(r.signature),
      });
    },
  };
})();
