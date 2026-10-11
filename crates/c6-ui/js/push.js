// Web push on this device: the service worker, the permission and the
// subscription. The Rust UI calls these and talks to the head.
(() => {
  const dec = (s) => Uint8Array.from(atob(s.replace(/-/g, "+").replace(/_/g, "/")), (c) => c.charCodeAt(0));

  window.c6Push = {
    // "unsupported" (no push here, or not a secure context; on iPhone and
    // iPad, only for the app added to the Home Screen), "denied",
    // "default" (not asked yet) or "granted".
    state() {
      if (!window.isSecureContext || !("serviceWorker" in navigator) || !("PushManager" in window) || !("Notification" in window))
        return "unsupported";
      return Notification.permission;
    },
    // This device's subscription as JSON, or "" if it has none.
    async current() {
      if (this.state() === "unsupported") return "";
      const reg = await navigator.serviceWorker.getRegistration("/");
      const sub = reg && (await reg.pushManager.getSubscription());
      return sub ? JSON.stringify(sub.toJSON()) : "";
    },
    // Asks for permission and subscribes with the head's key; the
    // subscription as JSON.
    async enable(key) {
      const reg = await navigator.serviceWorker.register("/sw.js", { scope: "/" });
      await navigator.serviceWorker.ready;
      if ((await Notification.requestPermission()) !== "granted") throw new Error("notifications are not allowed for this site");
      let sub = await reg.pushManager.getSubscription();
      if (!sub) sub = await reg.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: dec(key) });
      return JSON.stringify(sub.toJSON());
    },
    // Unsubscribes this device; its endpoint ("" if none).
    async disable() {
      const reg = await navigator.serviceWorker.getRegistration("/");
      const sub = reg && (await reg.pushManager.getSubscription());
      if (!sub) return "";
      const endpoint = sub.endpoint;
      await sub.unsubscribe();
      return endpoint;
    },
  };
})();
