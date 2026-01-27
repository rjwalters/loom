import React from "react";
import ReactDOM from "react-dom/client";
import { BrowserRouter } from "react-router-dom";
import { Toaster } from "@/components/ui/toaster";
import { DatabaseProvider } from "@/hooks/use-database";
import { ThemeProvider } from "@/hooks/use-theme";
import App from "./App";
import "./styles/globals.css";

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <BrowserRouter>
      <ThemeProvider>
        <DatabaseProvider>
          <App />
          <Toaster />
        </DatabaseProvider>
      </ThemeProvider>
    </BrowserRouter>
  </React.StrictMode>,
);
