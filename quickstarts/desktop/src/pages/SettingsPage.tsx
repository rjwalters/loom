import { useState, useEffect } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-shell";
import { useTheme } from "@/hooks/use-theme";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { ExternalLink, Folder } from "lucide-react";

export function SettingsPage() {
  const { theme, setTheme, resolvedTheme } = useTheme();
  const [appDataDir, setAppDataDir] = useState<string>("");

  useEffect(() => {
    invoke<string>("get_app_data_dir").then(setAppDataDir).catch(console.error);
  }, []);

  const openAppDataDir = async () => {
    if (appDataDir) {
      await open(appDataDir);
    }
  };

  return (
    <div className="space-y-6">
      <div>
        <h2 className="text-3xl font-bold tracking-tight">Settings</h2>
        <p className="text-muted-foreground">
          Configure your application preferences.
        </p>
      </div>

      <div className="grid gap-6">
        <Card>
          <CardHeader>
            <CardTitle>Appearance</CardTitle>
            <CardDescription>
              Customize how the application looks
            </CardDescription>
          </CardHeader>
          <CardContent className="space-y-4">
            <div>
              <label className="text-sm font-medium">Theme</label>
              <p className="text-sm text-muted-foreground mb-3">
                Select your preferred color scheme
              </p>
              <div className="flex gap-2">
                <Button
                  variant={theme === "light" ? "default" : "outline"}
                  onClick={() => setTheme("light")}
                >
                  Light
                </Button>
                <Button
                  variant={theme === "dark" ? "default" : "outline"}
                  onClick={() => setTheme("dark")}
                >
                  Dark
                </Button>
                <Button
                  variant={theme === "system" ? "default" : "outline"}
                  onClick={() => setTheme("system")}
                >
                  System
                </Button>
              </div>
              <p className="mt-2 text-xs text-muted-foreground">
                Currently using: {resolvedTheme} mode
              </p>
            </div>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>Data Storage</CardTitle>
            <CardDescription>
              Where your application data is stored
            </CardDescription>
          </CardHeader>
          <CardContent className="space-y-4">
            <div>
              <label className="text-sm font-medium">App Data Directory</label>
              <p className="text-sm text-muted-foreground mb-2">
                SQLite database and application settings
              </p>
              <div className="flex gap-2">
                <code className="flex-1 rounded-md bg-muted px-3 py-2 text-sm font-mono overflow-x-auto">
                  {appDataDir || "Loading..."}
                </code>
                <Button variant="outline" size="icon" onClick={openAppDataDir} disabled={!appDataDir}>
                  <Folder className="h-4 w-4" />
                </Button>
              </div>
            </div>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>About</CardTitle>
            <CardDescription>Application information</CardDescription>
          </CardHeader>
          <CardContent className="space-y-4">
            <div className="grid gap-2 text-sm">
              <div className="flex justify-between">
                <span className="text-muted-foreground">Version</span>
                <span>0.1.0</span>
              </div>
              <div className="flex justify-between">
                <span className="text-muted-foreground">Framework</span>
                <span>Tauri 2.0</span>
              </div>
              <div className="flex justify-between">
                <span className="text-muted-foreground">Frontend</span>
                <span>React 19, TypeScript</span>
              </div>
            </div>
            <div className="pt-2">
              <Button
                variant="outline"
                className="w-full"
                onClick={() => open("https://github.com/loomhq/loom")}
              >
                <ExternalLink className="mr-2 h-4 w-4" />
                View on GitHub
              </Button>
            </div>
          </CardContent>
        </Card>
      </div>
    </div>
  );
}
