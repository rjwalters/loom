import { Moon, Sun, Monitor } from "lucide-react";
import { useTheme } from "@/hooks/use-theme";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";

export function ThemeToggle() {
  const { theme, setTheme } = useTheme();

  return (
    <div className="flex items-center gap-1 rounded-md border border-border p-1">
      <Button
        variant="ghost"
        size="icon"
        className={cn("h-8 w-8", theme === "light" && "bg-accent")}
        onClick={() => setTheme("light")}
        title="Light mode"
      >
        <Sun className="h-4 w-4" />
      </Button>
      <Button
        variant="ghost"
        size="icon"
        className={cn("h-8 w-8", theme === "dark" && "bg-accent")}
        onClick={() => setTheme("dark")}
        title="Dark mode"
      >
        <Moon className="h-4 w-4" />
      </Button>
      <Button
        variant="ghost"
        size="icon"
        className={cn("h-8 w-8", theme === "system" && "bg-accent")}
        onClick={() => setTheme("system")}
        title="System preference"
      >
        <Monitor className="h-4 w-4" />
      </Button>
    </div>
  );
}
