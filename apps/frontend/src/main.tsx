import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App.tsx";
// Self-hosted type (OFL-1.1): only the weights the tokens use, bundled with the app so the UI
// makes no third-party font request (the CSP's font-src is 'self').
import "@fontsource/barlow/400.css";
import "@fontsource/barlow/500.css";
import "@fontsource/barlow/700.css";
import "@fontsource/barlow-condensed/400.css";
import "@fontsource/barlow-condensed/600.css";
import "./theme/tokens.css";

const root = document.getElementById("root");
if (!root) throw new Error("root element missing");

createRoot(root).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
