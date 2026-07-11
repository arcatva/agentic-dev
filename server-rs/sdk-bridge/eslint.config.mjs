// Flat ESLint config for the SDK bridge (correctness-oriented; style is left alone).
import js from "@eslint/js";
import globals from "globals";

export default [
  js.configs.recommended,
  {
    files: ["sdk-bridge.mjs"],
    languageOptions: {
      ecmaVersion: 2024,
      sourceType: "module",
      globals: { ...globals.node },
    },
    rules: {
      // The bridge logs through its own helpers but console is legitimate here.
      "no-console": "off",
      // Intentional empty catch blocks guard best-effort cleanup paths.
      "no-empty": ["error", { allowEmptyCatch: true }],
    },
  },
];
