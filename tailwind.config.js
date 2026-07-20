/** @type {import('tailwindcss').Config} */
export default {
  content: ["./index.html", "./src/**/*.{ts,tsx}"],
  theme: {
    extend: {
      colors: {
        // One clean default palette (matches xterm theme below).
        catty: {
          bg: "#1e1e2e",
          bg2: "#181825",
          fg: "#cdd6f4",
          muted: "#6c7086",
          accent: "#89b4fa",
          border: "#313244",
        },
      },
      fontFamily: {
        mono: [
          "Cascadia Code",
          "JetBrains Mono",
          "Fira Code",
          "Consolas",
          "monospace",
        ],
      },
    },
  },
  plugins: [],
};
