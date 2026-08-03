import{c as i,j as o,i as n}from"./vendor.mjs";import{i as c}from"./installWebviewStyles.mjs";const a=`
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
`;function s(e){let r=!1;return t=>{!t||r||(r=!0,e({type:"mount"}))}}function d(){const e=n.useRef(null);e.current||(e.current=s(t=>{window.webkit?.messageHandlers?.cmuxCode?.postMessage(t)}));const r=n.useCallback(t=>{e.current?.(t)},[]);return o.jsx("div",{className:"code-launcher",ref:r,children:o.jsx("div",{"aria-label":"Code",className:"code-launcher__spinner",role:"status"})})}function m(e){c("code",a),document.documentElement.dataset.cmuxWebviewKind="code",document.body.dataset.cmuxWebviewKind="code",i.createRoot(e).render(o.jsx(d,{}))}export{s as createCodeMountNotifier,m as mountCodeSurface};
