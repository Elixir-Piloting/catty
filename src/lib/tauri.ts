import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export interface TabInfo {
  id: number;
  cwd: string | null;
}

export const tabNew = (parentTabId?: number): Promise<TabInfo> =>
  invoke<TabInfo>("tab_new", parentTabId != null ? { parentTabId } : {});

export const tabClose = (id: number): Promise<void> =>
  invoke<void>("tab_close", { id });

export const tabInput = (id: number, data: string): Promise<void> =>
  invoke<void>("tab_input", { id, data });

export const tabResize = (id: number, cols: number, rows: number): Promise<void> =>
  invoke<void>("tab_resize", { id, cols, rows });

export const tabCwd = (id: number): Promise<string | null> =>
  invoke<string | null>("tab_cwd", { id });

export const historyAdd = (command: string): Promise<void> =>
  invoke<void>("history_add", { request: { command } });

export const historyMatch = (prefix: string): Promise<string | null> =>
  invoke<string | null>("history_match", { prefix });

export function onPtyData(
  tabId: number,
  cb: (b64: string) => void,
): Promise<UnlistenFn> {
  return listen<string>(`pty-data:${tabId}`, (e) => cb(e.payload));
}

export function onCwdChanged(
  tabId: number,
  cb: (cwd: string) => void,
): Promise<UnlistenFn> {
  return listen<string>(`cwd-changed:${tabId}`, (e) => cb(e.payload));
}

export function onPtyRestart(
  tabId: number,
  cb: () => void,
): Promise<UnlistenFn> {
  return listen<void>(`pty-restart:${tabId}`, () => cb());
}

export function onPtyExit(
  tabId: number,
  cb: () => void,
): Promise<UnlistenFn> {
  return listen<number>(`pty-exit:${tabId}`, () => cb());
}
