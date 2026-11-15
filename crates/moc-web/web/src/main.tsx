import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import "@fontsource-variable/geist";
import "@fontsource-variable/geist-mono";
import "katex/dist/katex.min.css";
import "streamdown/styles.css";
import "./index.css";
import { App } from "./App";
import { t } from "./lib/i18n";

const root = document.getElementById("root");
if (!root) throw new Error(t("main.no_root"));

createRoot(root).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
