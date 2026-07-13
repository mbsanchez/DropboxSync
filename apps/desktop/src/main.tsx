import React from "react";
import ReactDOM from "react-dom/client";
import { getCurrentWindow } from "@tauri-apps/api/window";
import ErrorBoundary from "./ErrorBoundary";
import SetupApp from "./components/SetupApp";
import FlyoutApp from "./components/FlyoutApp";

const label = getCurrentWindow().label;

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <ErrorBoundary>{label === "setup" ? <SetupApp /> : <FlyoutApp />}</ErrorBoundary>
  </React.StrictMode>,
);
