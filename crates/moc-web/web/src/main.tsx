import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import "@fontsource-variable/geist";
import "@fontsource-variable/geist-mono";
import "katex/dist/katex.min.css";
import "streamdown/styles.css";
import "./index.css";
import { App } from "./App";

const root = document.getElementById("root");
if (!root) throw new Error("index.html 缺少 #root");

createRoot(root).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
