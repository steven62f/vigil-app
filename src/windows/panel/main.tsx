import React from "react";
import ReactDOM from "react-dom/client";
import { PanelApp } from "./PanelApp";
import { SettingsProvider } from "../../hooks/SettingsProvider";
import "../../styles/globals.css";

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <SettingsProvider>
      <PanelApp />
    </SettingsProvider>
  </React.StrictMode>
);
