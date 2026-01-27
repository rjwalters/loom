import { invoke } from "@tauri-apps/api/core";
import { useState } from "react";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Input } from "@/components/ui/input";

export function HomePage() {
  const [name, setName] = useState("");
  const [greeting, setGreeting] = useState("");

  const handleGreet = async () => {
    if (!name.trim()) return;
    try {
      const result = await invoke<string>("greet", { name });
      setGreeting(result);
    } catch (error) {
      setGreeting(`Error: ${error}`);
    }
  };

  return (
    <div className="space-y-6">
      <div>
        <h2 className="text-3xl font-bold tracking-tight">Welcome</h2>
        <p className="text-muted-foreground">
          Your Loom Quickstart desktop application is ready.
        </p>
      </div>

      <div className="grid gap-6 md:grid-cols-2">
        <Card>
          <CardHeader>
            <CardTitle>Tauri IPC Demo</CardTitle>
            <CardDescription>
              Test the Tauri invoke command to call Rust backend
            </CardDescription>
          </CardHeader>
          <CardContent className="space-y-4">
            <div className="flex gap-2">
              <Input
                placeholder="Enter your name"
                value={name}
                onChange={(e) => setName(e.target.value)}
                onKeyDown={(e) => e.key === "Enter" && handleGreet()}
              />
              <Button onClick={handleGreet}>Greet</Button>
            </div>
            {greeting && (
              <p className="rounded-md bg-muted p-3 text-sm">{greeting}</p>
            )}
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>Features</CardTitle>
            <CardDescription>What's included in this template</CardDescription>
          </CardHeader>
          <CardContent>
            <ul className="space-y-2 text-sm">
              <li className="flex items-center gap-2">
                <span className="h-2 w-2 rounded-full bg-green-500" />
                System tray with context menu
              </li>
              <li className="flex items-center gap-2">
                <span className="h-2 w-2 rounded-full bg-green-500" />
                Local SQLite database
              </li>
              <li className="flex items-center gap-2">
                <span className="h-2 w-2 rounded-full bg-green-500" />
                Dark/light theme switching
              </li>
              <li className="flex items-center gap-2">
                <span className="h-2 w-2 rounded-full bg-green-500" />
                Native window management
              </li>
              <li className="flex items-center gap-2">
                <span className="h-2 w-2 rounded-full bg-green-500" />
                Cross-platform builds
              </li>
            </ul>
          </CardContent>
        </Card>
      </div>

      <Card>
        <CardHeader>
          <CardTitle>Getting Started</CardTitle>
          <CardDescription>Quick start guide for development</CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          <div className="rounded-md bg-muted p-4 font-mono text-sm">
            <p className="text-muted-foreground"># Install dependencies</p>
            <p>pnpm install</p>
            <br />
            <p className="text-muted-foreground"># Start development</p>
            <p>pnpm tauri dev</p>
            <br />
            <p className="text-muted-foreground"># Build for production</p>
            <p>pnpm tauri build</p>
          </div>
        </CardContent>
      </Card>
    </div>
  );
}
