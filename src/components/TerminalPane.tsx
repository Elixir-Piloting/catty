import { useEffect, useRef } from "react";
import { Terminal as XTerm } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebLinksAddon } from "@xterm/addon-web-links";
import { WebglAddon } from "@xterm/addon-webgl";
import "@xterm/xterm/css/xterm.css";

import {
  historyAdd,
  historyMatch,
  onCwdChanged,
  onPtyData,
  onPtyExit,
  onPtyRestart,
  tabInput,
  tabResize,
} from "../lib/tauri";
import {
  applyKeystroke,
  emptyLine,
  ghostSuffix,
  type LineState,
} from "../lib/inputLine";

interface TerminalPaneProps {
  tabId: number;
  active: boolean;
  onCwd: (cwd: string) => void;
}

const FONT_FAMILY =
  '"Cascadia Code", "JetBrains Mono", "Fira Code", Consolas, monospace';
const FONT_SIZE = 13;

const THEME = {
  background: "#1e1e2e",
  foreground: "#cdd6f4",
  cursor: "#f5e0dc",
  cursorAccent: "#1e1e2e",
  selectionBackground: "#585b7066",
  black: "#45475a",
  red: "#f38ba8",
  green: "#a6e3a1",
  yellow: "#f9e2af",
  blue: "#89b4fa",
  magenta: "#f5c2e7",
  cyan: "#94e2d5",
  white: "#bac2de",
  brightBlack: "#585b70",
  brightRed: "#f38ba8",
  brightGreen: "#a6e3a1",
  brightYellow: "#f9e2af",
  brightBlue: "#89b4fa",
  brightMagenta: "#f5c2e7",
  brightCyan: "#94e2d5",
  brightWhite: "#a6adc8",
};

export default function TerminalPane({
  tabId,
  active,
  onCwd,
}: TerminalPaneProps) {
  const containerRef = useRef<HTMLDivElement>(null);
  const termRef = useRef<XTerm | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const overlayRef = useRef<HTMLDivElement>(null);
  const lineRef = useRef<LineState>(emptyLine());
  const suggestionRef = useRef<string | null>(null);
  const matchReqIdRef = useRef(0);

  useEffect(() => {
    console.log(`[catty tab ${tabId}] TerminalPane MOUNT`);
    if (!containerRef.current) return;

    // Suppress clear-screen/scrollback sequences from the new shell's startup
    // output so xterm.js preserves its buffer across a respawn.
    let suppressClear = false;
    let clearSuppressTimer: ReturnType<typeof setTimeout> | null = null;

    const term = new XTerm({
      fontFamily: FONT_FAMILY,
      fontSize: FONT_SIZE,
      cursorBlink: true,
      scrollback: 5000,
      allowProposedApi: true,
      theme: THEME,
    });
    termRef.current = term;

    const fit = new FitAddon();
    term.loadAddon(fit);
    fitRef.current = fit;
    term.loadAddon(new WebLinksAddon());
    try {
      term.loadAddon(new WebglAddon());
    } catch {
      // WebGL renderer unavailable (e.g. headless); fall back to canvas DOM.
    }

    term.open(containerRef.current);
    try {
      fit.fit();
    } catch {
      // container not visible yet; ResizeObserver will retry.
    }

    // Initial resize to backend.
    tabResize(tabId, term.cols, term.rows).catch(() => {});

    // --- Ghost-text overlay element ---------------------------------------
    const overlay = document.createElement("div");
    overlay.style.position = "absolute";
    overlay.style.color = "rgba(205, 214, 244, 0.35)";
    overlay.style.fontFamily = FONT_FAMILY;
    overlay.style.fontSize = `${FONT_SIZE}px`;
    overlay.style.lineHeight = "1";
    overlay.style.whiteSpace = "pre";
    overlay.style.pointerEvents = "none";
    overlay.style.userSelect = "none";
    overlay.style.display = "none";
    overlay.style.zIndex = "5";
    containerRef.current.appendChild(overlay);
    overlayRef.current = overlay;

    const updateOverlay = () => {
      const ov = overlayRef.current;
      const cont = containerRef.current;
      const t = termRef.current;
      if (!ov || !cont || !t) return;

      const buffer = lineRef.current.buffer;
      const cursor = lineRef.current.cursor;
      const suffix = ghostSuffix(buffer, suggestionRef.current);

      // Only show when the cursor is at the end of the line.
      if (!suffix || cursor !== buffer.length) {
        ov.style.display = "none";
        return;
      }

      const rowsEl = cont.querySelector<HTMLElement>(".xterm-rows");
      if (!rowsEl) return;
      const contRect = cont.getBoundingClientRect();
      const rowsRect = rowsEl.getBoundingClientRect();
      const cellW = rowsRect.width / t.cols;
      const cellH = rowsRect.height / t.rows;
      const cursorX = t.buffer.active.cursorX;
      const cursorY = t.buffer.active.cursorY;

      ov.style.left = `${rowsRect.left - contRect.left + cursorX * cellW}px`;
      ov.style.top = `${rowsRect.top - contRect.top + cursorY * cellH}px`;
      ov.style.maxWidth = `${(t.cols - cursorX) * cellW}px`;
      ov.style.overflow = "hidden";
      ov.textContent = suffix;
      ov.style.display = "block";
    };

    const refreshSuggestion = () => {
      const buffer = lineRef.current.buffer;
      const reqId = ++matchReqIdRef.current;
      if (buffer.length === 0) {
        suggestionRef.current = null;
        updateOverlay();
        return;
      }
      historyMatch(buffer)
        .then((match) => {
          if (reqId !== matchReqIdRef.current) return; // stale
          suggestionRef.current = match;
          updateOverlay();
        })
        .catch(() => {});
    };

    // --- Keystroke handling ------------------------------------------------
    const disposers: Array<() => void> = [];

    disposers.push(
      term.onData((data) => {
        const res = applyKeystroke(lineRef.current, data);

        if (res.rightArrow) {
          const suffix = ghostSuffix(
            lineRef.current.buffer,
            suggestionRef.current,
          );
          if (suffix && lineRef.current.cursor === lineRef.current.buffer.length) {
            // Accept the suggestion: send the suffix to the shell.
            tabInput(tabId, suffix).catch(() => {});
            lineRef.current = {
              buffer: lineRef.current.buffer + suffix,
              cursor: lineRef.current.buffer.length + suffix.length,
            };
            suggestionRef.current = null;
            updateOverlay();
            return;
          }
          // No suggestion: pass the arrow through and advance local cursor.
          tabInput(tabId, data).catch(() => {});
          lineRef.current = {
            buffer: lineRef.current.buffer,
            cursor: Math.min(lineRef.current.buffer.length, lineRef.current.cursor + 1),
          };
          updateOverlay();
          return;
        }

        if (res.tab) {
          const suffix = ghostSuffix(
            lineRef.current.buffer,
            suggestionRef.current,
          );
          if (suffix && lineRef.current.cursor === lineRef.current.buffer.length) {
            tabInput(tabId, suffix).catch(() => {});
            lineRef.current = {
              buffer: lineRef.current.buffer + suffix,
              cursor: lineRef.current.buffer.length + suffix.length,
            };
            suggestionRef.current = null;
            updateOverlay();
            return;
          }
          // No suggestion: let the shell do completion.
          tabInput(tabId, data).catch(() => {});
          return;
        }

        // Regular keystroke: forward to shell, update local state.
        tabInput(tabId, data).catch(() => {});
        lineRef.current = res.state;

        if (res.committed) {
          historyAdd(res.committed).catch(() => {});
          suggestionRef.current = null;
          updateOverlay();
          return;
        }
        refreshSuggestion();
      }).dispose,
    );

    // --- Backend -> terminal: PTY output -----------------------------------
    const dec = new TextDecoder();
    const onDataUnlistenP = onPtyData(tabId, (b64) => {
      try {
        const bin = atob(b64);
        // Detect clear sequences
        if (bin.includes("\x1b[2J") || bin.includes("\x1b[3J") || bin.includes("\x1bc")) {
          console.log(`[catty tab ${tabId}] CLEAR SEQ detected in pty-data (len=${bin.length})`);
        }
        let cleanBin = bin;
        if (suppressClear) {
          // Strip sequences that would clobber the terminal buffer
          // when a new shell starts after a respawn.
          cleanBin = cleanBin
            .replace(/\x1b\[3J/g, "")   // erase scrollback
            .replace(/\x1b\[2J/g, "")   // erase display
            .replace(/\x1bc/g, "")      // RIS (reset)
            .replace(/\x1b\[H/g, "");   // cursor home (harmless alone but included in clear seqs)
        }
        const bytes = new Uint8Array(cleanBin.length);
        for (let i = 0; i < cleanBin.length; i++) bytes[i] = cleanBin.charCodeAt(i);
        term.write(dec.decode(bytes, { stream: true }));
        // Output may have moved the cursor; reposition overlay.
        requestAnimationFrame(updateOverlay);
      } catch (err) {
        console.error("pty-data decode failed", err);
      }
    });

    const onCwdUnlistenP = onCwdChanged(tabId, (cwd) => {
      onCwd(cwd);
      requestAnimationFrame(updateOverlay);
    });

    const onRestartUnlistenP = onPtyRestart(tabId, () => {
      console.log(`[catty tab ${tabId}] pty-restart event, resetting input line`);
      lineRef.current = emptyLine();
      suggestionRef.current = null;
      suppressClear = true;
      clearSuppressTimer = setTimeout(() => {
        suppressClear = false;
      }, 2000);
      requestAnimationFrame(updateOverlay);
    });

    const onExitUnlistenP = onPtyExit(tabId, () => {
      console.log(`[catty tab ${tabId}] pty-exit event (fatal), not respawning`);
    });

    // --- Resize handling ---------------------------------------------------
    const ro = new ResizeObserver(() => {
      const cont = containerRef.current;
      if (!cont || cont.clientWidth === 0 || cont.clientHeight === 0) return;
      try {
        fit.fit();
      } catch {
        return; // not measured yet
      }
      const t = termRef.current;
      if (t) tabResize(tabId, t.cols, t.rows).catch(() => {});
      updateOverlay();
    });
    ro.observe(containerRef.current);

    // --- Cleanup -----------------------------------------------------------
    return () => {
      console.log(`[catty tab ${tabId}] TerminalPane UNMOUNT`);
      if (clearSuppressTimer != null) clearTimeout(clearSuppressTimer);
      ro.disconnect();
      disposers.forEach((d) => d());
      onDataUnlistenP.then((u) => u());
      onCwdUnlistenP.then((u) => u());
      onRestartUnlistenP.then((u) => u());
      onExitUnlistenP.then((u) => u());
      overlay.remove();
      term.dispose();
      termRef.current = null;
      overlayRef.current = null;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tabId]);

  // When becoming active, refit (in case the container was hidden) and focus.
  useEffect(() => {
    if (!active) return;
    const t = termRef.current;
    const f = fitRef.current;
    if (t && f) {
      try {
        f.fit();
        tabResize(tabId, t.cols, t.rows).catch(() => {});
      } catch {
        // ignore fit failures during transitions
      }
      requestAnimationFrame(() => t.focus());
    }
  }, [active, tabId]);

  return (
    <div
      ref={containerRef}
      className="h-full w-full"
      style={{ display: active ? "block" : "none" }}
    />
  );
}
