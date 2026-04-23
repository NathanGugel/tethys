import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { ErrorBoundary, ErrorOverlay } from "./ErrorOverlay";

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <ErrorBoundary>
      <App />
    </ErrorBoundary>
    <ErrorOverlay />
  </React.StrictMode>,
);
