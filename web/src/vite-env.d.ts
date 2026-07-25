/// <reference types="vite/client" />

// Vite resolves CSS side-effect imports at build time; TypeScript needs to be
// told they exist.
declare module "*.css";
