import { Link } from "react-router-dom";
import { useAuth } from "@/hooks/use-auth";
import { Button } from "@/components/ui/button";
import { ThemeToggle } from "@/components/ThemeToggle";

export function Layout({ children }: { children: React.ReactNode }) {
  const { user, logout } = useAuth();

  return (
    <div className="min-h-screen bg-background">
      <header className="sticky top-0 z-50 w-full border-b bg-background/95 backdrop-blur supports-[backdrop-filter]:bg-background/60">
        <div className="container flex h-14 items-center">
          <div className="mr-4 flex">
            <Link to="/" className="mr-6 flex items-center space-x-2">
              <span className="font-bold">Loom Quickstart</span>
            </Link>
            <nav className="flex items-center space-x-6 text-sm font-medium">
              <Link
                to="/"
                className="transition-colors hover:text-foreground/80 text-foreground/60"
              >
                Home
              </Link>
              {user && (
                <Link
                  to="/dashboard"
                  className="transition-colors hover:text-foreground/80 text-foreground/60"
                >
                  Dashboard
                </Link>
              )}
            </nav>
          </div>
          <div className="flex flex-1 items-center justify-end space-x-2">
            <ThemeToggle />
            {user ? (
              <div className="flex items-center gap-4">
                <span className="text-sm text-muted-foreground">{user.name}</span>
                <Button variant="outline" size="sm" onClick={() => logout()}>
                  Logout
                </Button>
              </div>
            ) : (
              <Button asChild variant="default" size="sm">
                <Link to="/login">Login</Link>
              </Button>
            )}
          </div>
        </div>
      </header>
      <main className="container py-6">{children}</main>
    </div>
  );
}
