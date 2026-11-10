import React, { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { SettingsApp } from "../settings/SettingsApp";
import { UsageApp } from "../usage/UsageApp";
import { HistoryApp } from "../history/HistoryApp";
import { StatsApp } from "../stats/StatsApp";
import { InstallationPage } from "./pages/InstallationPage";

type Page = "settings" | "usage" | "history" | "statistics" | "installation";

interface NavItem {
  page: Page;
  label: string;
  icon: React.ReactNode;
}

const ICON_PROPS = {
  width: 20,
  height: 20,
  viewBox: "0 0 24 24",
  fill: "none",
  stroke: "currentColor",
  strokeWidth: 2,
  strokeLinecap: "round" as const,
  strokeLinejoin: "round" as const,
};

const IconGear = () => (
  <svg {...ICON_PROPS}>
    <circle cx="12" cy="12" r="3" />
    <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" />
  </svg>
);

const IconChart = () => (
  <svg {...ICON_PROPS}>
    <path d="M3 3v18h18" />
    <path d="M7 14l4-4 4 4 5-5" />
  </svg>
);

const IconClock = () => (
  <svg {...ICON_PROPS}>
    <circle cx="12" cy="12" r="9" />
    <path d="M12 7v5l3 2" />
  </svg>
);

const IconBarChart = () => (
  <svg {...ICON_PROPS}>
    <line x1="6" y1="20" x2="6" y2="12" />
    <line x1="12" y1="20" x2="12" y2="4" />
    <line x1="18" y1="20" x2="18" y2="8" />
  </svg>
);

const IconInstall = () => (
  <svg {...ICON_PROPS}>
    <path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" />
    <polyline points="7 10 12 15 17 10" />
    <line x1="12" y1="15" x2="12" y2="3" />
  </svg>
);

const NAV_ITEMS: readonly NavItem[] = [
  { page: "settings",     label: "Settings",     icon: <IconGear /> },
  { page: "usage",        label: "Usage",         icon: <IconChart /> },
  { page: "history",      label: "History",       icon: <IconClock /> },
  { page: "statistics",   label: "Statistics",    icon: <IconBarChart /> },
  { page: "installation", label: "Installation",  icon: <IconInstall /> },
];

function SidebarItem({
  item,
  active,
  onClick,
}: {
  item: NavItem;
  active: boolean;
  onClick: () => void;
}) {
  const [hovered, setHovered] = useState(false);

  return (
    <button
      type="button"
      onClick={onClick}
      onMouseEnter={() => setHovered(true)}
      onMouseLeave={() => setHovered(false)}
      style={{
        display: "flex",
        alignItems: "center",
        gap: 10,
        padding: "10px 16px",
        background: active
          ? "var(--bg-subtle)"
          : hovered
            ? "var(--bg-surface-hover)"
            : "transparent",
        color: active ? "var(--text-primary)" : "var(--text-secondary)",
        border: "none",
        borderLeft: active
          ? "3px solid var(--color-amber)"
          : "3px solid transparent",
        cursor: "pointer",
        fontSize: 13,
        fontFamily: "inherit",
        textAlign: "left",
        width: "100%",
        boxSizing: "border-box",
      }}
    >
      <span style={{ display: "inline-flex" }}>{item.icon}</span>
      <span>{item.label}</span>
    </button>
  );
}

function Sidebar({
  currentPage,
  onNavigate,
}: {
  currentPage: Page;
  onNavigate: (p: Page) => void;
}) {
  return (
    <nav
      style={{
        width: 200,
        flexShrink: 0,
        background: "var(--bg-surface)",
        borderRight: "1px solid var(--border-subtle)",
        display: "flex",
        flexDirection: "column",
        padding: "12px 0",
        gap: 2,
      }}
    >
      {NAV_ITEMS.map((item) => (
        <SidebarItem
          key={item.page}
          item={item}
          active={item.page === currentPage}
          onClick={() => onNavigate(item.page)}
        />
      ))}
    </nav>
  );
}

export function PanelApp() {
  const [currentPage, setCurrentPage] = useState<Page>("settings");

  useEffect(() => {
    invoke<boolean>("get_installation_required")
      .catch(() => true)
      .then((required) => {
        if (required) setCurrentPage("installation");
      });
  }, []);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    listen<{ page: Page }>("vigil:navigate", async (event) => {
      const required = await invoke<boolean>("get_installation_required").catch(() => true);
      if (required) {
        setCurrentPage("installation");
      } else {
        setCurrentPage(event.payload.page ?? "settings");
      }
    }).then((fn) => {
      if (cancelled) fn();
      else unlisten = fn;
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);

  return (
    <div
      style={{
        display: "flex",
        height: "100vh",
        background: "var(--bg-overlay)",
        color: "var(--text-primary)",
      }}
    >
      <Sidebar currentPage={currentPage} onNavigate={setCurrentPage} />
      <div style={{ flex: 1, overflow: "hidden", minWidth: 0 }}>
        {currentPage === "settings"     && <SettingsApp       key="settings" />}
        {currentPage === "usage"        && <UsageApp          key="usage" />}
        {currentPage === "history"      && <HistoryApp        key="history" />}
        {currentPage === "statistics"   && <StatsApp          key="statistics" />}
        {currentPage === "installation" && <InstallationPage  key="installation" />}
      </div>
    </div>
  );
}
