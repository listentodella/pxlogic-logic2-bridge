'use strict';

const { contextBridge, ipcRenderer } = require('electron');

contextBridge.exposeInMainWorld('pxlogicBridge', {
  initialState: () => ipcRenderer.invoke('client:initial-state'),
  saveSettings: settings => ipcRenderer.invoke('client:save-settings', settings),
  scanLogicApps: savedPath => ipcRenderer.invoke('logic:scan', savedPath),
  inspectLogicApp: appPath => ipcRenderer.invoke('logic:inspect', appPath),
  browseLogicApp: () => ipcRenderer.invoke('logic:browse'),
  start: settings => ipcRenderer.invoke('bridge:start', settings),
  stop: () => ipcRenderer.invoke('bridge:stop'),
  openLogs: () => ipcRenderer.invoke('logs:open'),
  onState: callback => ipcRenderer.on('bridge:state', (_event, state) => callback(state)),
  onLog: callback => ipcRenderer.on('bridge:log', (_event, line) => callback(line)),
});
