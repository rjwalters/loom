import { Link, useLocation } from "react-router-dom";
import { Home, FileText, Settings } from "lucide-react";
import { cn } from "@/lib/utils";
import { ThemeToggle } from "./ThemeToggle";

const navigation = [
  { name: "Home", href: "/", icon: Home },
  { name: "Notes", href: "/notes", icon: FileText },
  { name: "Settings", href: "/settings", icon: Settings },
];

export function Layout({ children }: { children: React.ReactNode }) {
  const location = useLocation();

  return (
    <div className="flex h-screen">
      {/* Sidebar */}
      <div className="flex w-64 flex-col border-r border-border bg-muted/30">
        {/* App title - draggable region for window */}
        <div
          className="flex h-14 items-center border-b border-border px-4"
          data-tauri-drag-region
        >
          <h1 className="text-lg font-semibold">Loom Quickstart</h1>
        </div>

        {/* Navigation */}
        <nav className="flex-1 space-y-1 p-2">
          {navigation.map((item) => {
            const isActive = location.pathname === item.href;
            return (
              <Link
                key={item.name}
                to={item.href}
                className={cn(
                  "flex items-center gap-3 rounded-md px-3 py-2 text-sm font-medium transition-colors",
                  isActive
                    ? "bg-primary text-primary-foreground"
                    : "text-muted-foreground hover:bg-accent hover:text-accent-foreground"
                )}
              >
                <item.icon className="h-4 w-4" />
                {item.name}
              </Link>
            );
          })}
        </nav>

        {/* Footer with theme toggle */}
        <div className="border-t border-border p-4">
          <ThemeToggle />
        </div>
      </div>

      {/* Main content */}
      <div className="flex flex-1 flex-col">
        {/* Draggable title bar region */}
        <div
          className="h-8 shrink-0 border-b border-border"
          data-tauri-drag-region
        />

        {/* Content area */}
        <main className="flex-1 overflow-auto p-6 selectable">{children}</main>
      </div>
    </div>
  );
}
