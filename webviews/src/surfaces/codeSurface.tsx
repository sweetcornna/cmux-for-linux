import { useCallback, useRef } from "react";
import { createRoot } from "react-dom/client";
import { installWebviewStyles } from "./installWebviewStyles";

const codeStyles = `
:root { color-scheme: light dark; }
html, body, #root { width: 100%; height: 100%; margin: 0; }
body {
  background: light-dark(#fcfcfc, #0e0e0e);
  color: light-dark(#262626, #f5f5f5);
  font-family: -apple-system, BlinkMacSystemFont, "SF Pro Text", sans-serif;
}
.code-launcher {
  align-items: center;
  display: flex;
  height: 100%;
  justify-content: center;
}
.code-launcher__spinner {
  animation: code-launcher-spin 850ms linear infinite;
  border: 2px solid light-dark(rgb(38 38 38 / 16%), rgb(245 245 245 / 16%));
  border-radius: 999px;
  border-top-color: light-dark(#526fff, #6073cc);
  height: 18px;
  width: 18px;
}
@keyframes code-launcher-spin { to { transform: rotate(360deg); } }
@media (prefers-reduced-motion: reduce) { .code-launcher__spinner { animation: none; } }
`;

export function createCodeMountNotifier(postMessage: (message: unknown) => void): (node: HTMLElement | null) => void {
  let mounted = false;
  return (node) => {
    if (!node || mounted) return;
    mounted = true;
    postMessage({ type: "mount" });
  };
}

function CodeLauncher() {
  const notifier = useRef<ReturnType<typeof createCodeMountNotifier> | null>(null);
  if (!notifier.current) {
    notifier.current = createCodeMountNotifier((message) => {
      window.webkit?.messageHandlers?.cmuxCode?.postMessage(message);
    });
  }
  const mountRef = useCallback((node: HTMLDivElement | null) => {
    notifier.current?.(node);
  }, []);
  return (
    <div className="code-launcher" ref={mountRef}>
      <div aria-label="Code" className="code-launcher__spinner" role="status" />
    </div>
  );
}

export function mountCodeSurface(rootElement: HTMLElement): void {
  installWebviewStyles("code", codeStyles);
  document.documentElement.dataset.cmuxWebviewKind = "code";
  document.body.dataset.cmuxWebviewKind = "code";
  createRoot(rootElement).render(<CodeLauncher />);
}
