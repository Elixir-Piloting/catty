import { useCallback, useEffect, useMemo, useState } from "react";
import TerminalPane from "./components/TerminalPane";
import { tabClose, tabNew } from "./lib/tauri";

interface Tab {
  id: number;
  cwd: string | null;
}

function folderName(cwd: string | null): string {
  if (!cwd) return "catty";
  const cleaned = cwd.replace(/\/+$/, "").replace(/\\+$/, "");
  if (!cleaned) return "catty";
  const parts = cleaned.split(/[\\/]/).filter(Boolean);
  if (parts.length === 0) return "catty";
  // Drive root like "C:" -> show as-is.
  return parts[parts.length - 1] || "catty";
}

export default function App() {
  const [tabs, setTabs] = useState<Tab[]>([]);
  const [activeId, setActiveId] = useState<number | null>(null);

  const openTab = useCallback(
    async (parentId?: number) => {
      const info = await tabNew(parentId);
      const tab: Tab = { id: info.id, cwd: info.cwd };
      setTabs((prev) => [...prev, tab]);
      setActiveId(info.id);
    },
    [],
  );

  const closeTab = useCallback(
    async (id: number) => {
      await tabClose(id).catch(() => {});
      setTabs((prev) => {
        const next = prev.filter((t) => t.id !== id);
        setActiveId((curr) => {
          if (curr !== id) return curr;
          const idx = prev.findIndex((t) => t.id === id);
          const neighbor = next[idx] ?? next[idx - 1] ?? null;
          return neighbor ? neighbor.id : null;
        });
        return next;
      });
    },
    [],
  );

  // Boot the first tab on mount.
  useEffect(() => {
    openTab().catch((e) => console.error("failed to open initial tab", e));
  }, [openTab]);

  // Global keyboard shortcuts.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      // Ctrl+T: new tab (inherit active tab's cwd).
      if (e.ctrlKey && !e.shiftKey && !e.altKey && e.key === "t") {
        e.preventDefault();
        openTab(activeId ?? undefined);
        return;
      }
      // Ctrl+W: close active tab.
      if (e.ctrlKey && !e.shiftKey && !e.altKey && e.key === "w") {
        e.preventDefault();
        if (activeId != null) closeTab(activeId);
        return;
      }
      // Ctrl+Tab: cycle to next tab.
      if (e.ctrlKey && e.key === "Tab") {
        e.preventDefault();
        setTabs((prev) => {
          if (prev.length < 2) return prev;
          const idx = prev.findIndex((t) => t.id === activeId);
          const nextIdx = e.shiftKey
            ? (idx - 1 + prev.length) % prev.length
            : (idx + 1) % prev.length;
          setActiveId(prev[nextIdx].id);
          return prev;
        });
        return;
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [activeId, openTab, closeTab]);

  const setCwd = useCallback((id: number, cwd: string) => {
    setTabs((prev) =>
      prev.map((t) => (t.id === id ? { ...t, cwd } : t)),
    );
  }, []);

  const activeIndex = useMemo(
    () => tabs.findIndex((t) => t.id === activeId),
    [tabs, activeId],
  );

  return (
    <div className="flex h-screen w-screen flex-col bg-catty-bg text-catty-fg">
      {/* Tab bar */}
      <div className="flex shrink-0 items-center border-b border-catty-border bg-catty-bg2">
        <div className="flex flex-1 items-stretch overflow-x-auto">
          {tabs.map((tab, i) => {
            const isActive = tab.id === activeId;
            return (
              <div
                key={tab.id}
                onClick={() => setActiveId(tab.id)}
                className={
                  "group flex cursor-pointer items-center gap-2 px-3 py-2 text-xs " +
                  "border-r border-catty-border select-none " +
                  (isActive
                    ? "bg-catty-bg text-catty-fg"
                    : "bg-catty-bg2 text-catty-muted hover:bg-catty-bg/60")
                }
              >
                <span className="font-mono">{folderName(tab.cwd)}</span>
                <span className="text-catty-muted">#{i + 1}</span>
                <button
                  onClick={(e) => {
                    e.stopPropagation();
                    closeTab(tab.id);
                  }}
                  className="ml-1 rounded px-1 text-catty-muted hover:bg-catty-border hover:text-catty-fg"
                  title="Close tab"
                >
                  ×
                </button>
              </div>
            );
          })}
        </div>
        <button
          onClick={() => openTab(activeId ?? undefined)}
          className="shrink-0 px-3 py-2 text-catty-muted hover:bg-catty-bg/60 hover:text-catty-fg"
          title="New tab (Ctrl+T)"
        >
          +
        </button>
      </div>

      {/* Terminal panes */}
      <div className="relative flex-1">
        {tabs.length === 0 && (
          <div className="flex h-full items-center justify-center text-catty-muted text-sm">
            No tabs. Press Ctrl+T to open one.
          </div>
        )}
        {tabs.map((tab) => (
          <div
            key={tab.id}
            className="absolute inset-0"
            style={{ display: tab.id === activeId ? "block" : "none" }}
          >
            <TerminalPane
              tabId={tab.id}
              active={tab.id === activeId}
              onCwd={(cwd) => setCwd(tab.id, cwd)}
            />
          </div>
        ))}
      </div>

      {/* Status strip */}
      <div className="flex shrink-0 items-center justify-between border-t border-catty-border bg-catty-bg2 px-3 py-1 text-[11px] text-catty-muted">
        <span>{tabs[activeIndex]?.cwd ?? "~"}</span>
        <span>catty</span>
      </div>
    </div>
  );
}
