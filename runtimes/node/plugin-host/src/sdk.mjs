export function createPluginApi(boundBridge) {
  if (!boundBridge || typeof boundBridge.call !== 'function' || typeof boundBridge.notify !== 'function') throw new TypeError('boundBridge is invalid');
  return Object.freeze({
    call(method, payload, options = {}) { return boundBridge.call(method, payload, options); },
    notify(method, payload) { return boundBridge.notify(method, payload); },
  });
}
