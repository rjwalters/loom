import { Link } from "react-router-dom";
import { useAuth } from "@/hooks/use-auth";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";

export function HomePage() {
  const { user } = useAuth();

  return (
    <div className="flex flex-col items-center justify-center space-y-8 py-12">
      <div className="text-center space-y-4">
        <h1 className="text-4xl font-bold tracking-tight sm:text-5xl">
          Loom Quickstart Webapp
        </h1>
        <p className="max-w-[600px] text-lg text-muted-foreground">
          A modern web application template with Cloudflare Workers, Vite, React, Tailwind CSS,
          and shadcn/ui. Pre-configured with authentication, theming, and D1 database.
        </p>
      </div>

      <div className="flex gap-4">
        {user ? (
          <Button asChild size="lg">
            <Link to="/dashboard">Go to Dashboard</Link>
          </Button>
        ) : (
          <>
            <Button asChild size="lg">
              <Link to="/login">Get Started</Link>
            </Button>
            <Button asChild variant="outline" size="lg">
              <a href="https://github.com/rjwalters/loom" target="_blank" rel="noopener noreferrer">
                Learn More
              </a>
            </Button>
          </>
        )}
      </div>

      <div className="grid gap-6 sm:grid-cols-2 lg:grid-cols-3 mt-12">
        <Card>
          <CardHeader>
            <CardTitle>Authentication</CardTitle>
            <CardDescription>Secure user authentication flow ready to customize</CardDescription>
          </CardHeader>
          <CardContent>
            <p className="text-sm text-muted-foreground">
              Login, logout, and registration with session management. Connect to your preferred
              auth provider.
            </p>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>Dark Mode</CardTitle>
            <CardDescription>Light and dark theme with system preference support</CardDescription>
          </CardHeader>
          <CardContent>
            <p className="text-sm text-muted-foreground">
              Automatic theme detection with manual toggle. Persists user preference across
              sessions.
            </p>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>D1 Database</CardTitle>
            <CardDescription>Cloudflare D1 SQLite database integration</CardDescription>
          </CardHeader>
          <CardContent>
            <p className="text-sm text-muted-foreground">
              Pre-configured schema with migrations. Serverless SQL at the edge with global
              replication.
            </p>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>Loom Ready</CardTitle>
            <CardDescription>Pre-configured for Loom AI orchestration</CardDescription>
          </CardHeader>
          <CardContent>
            <p className="text-sm text-muted-foreground">
              Includes Loom roles and configuration. Start building features with AI-powered
              development.
            </p>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>Modern Stack</CardTitle>
            <CardDescription>React 19, Vite, Tailwind CSS 4, TypeScript</CardDescription>
          </CardHeader>
          <CardContent>
            <p className="text-sm text-muted-foreground">
              Latest tools for fast development. Hot module replacement, type safety, and modern
              styling.
            </p>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>Edge Deployment</CardTitle>
            <CardDescription>Deploy globally on Cloudflare Pages</CardDescription>
          </CardHeader>
          <CardContent>
            <p className="text-sm text-muted-foreground">
              Sub-millisecond latency worldwide. Automatic SSL, DDoS protection, and infinite
              scalability.
            </p>
          </CardContent>
        </Card>
      </div>
    </div>
  );
}
