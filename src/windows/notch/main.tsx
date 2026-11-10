import React from "react";
import ReactDOM from "react-dom/client";
import { NotchApp } from "./NotchApp";
import "../../styles/globals.css";
import "../../styles/notch.css";

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <NotchApp />
  </React.StrictMode>
);
