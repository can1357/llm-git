const { contextBridge, ipcRenderer } = require("electron");

contextBridge.exposeInMainWorld("reel", {
  getState: () => ipcRenderer.invoke("getState"),
  onState: (cb) => ipcRenderer.on("state", (_event, state) => cb(state)),
  act: (action) => ipcRenderer.send("act", action),
});
