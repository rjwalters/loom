import { useCallback, useEffect, useState } from "react";
import { EmptyState } from "@/components/EmptyState";
import { type Project, ProjectCard } from "@/components/ProjectCard";
import { ProjectForm } from "@/components/ProjectForm";
import { Button } from "@/components/ui/button";
import { useAuth } from "@/hooks/use-auth";

export function ProjectsPage() {
  const { user } = useAuth();
  const [projects, setProjects] = useState<Project[]>([]);
  const [isLoading, setIsLoading] = useState(true);
  const [showCreateForm, setShowCreateForm] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const fetchProjects = useCallback(async () => {
    if (!user) return;

    try {
      const response = await fetch("/api/projects", {
        headers: { "X-User-Id": user.id },
      });
      if (!response.ok) throw new Error("Failed to fetch projects");
      const data = await response.json();
      setProjects(data.projects || []);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to load projects");
    } finally {
      setIsLoading(false);
    }
  }, [user]);

  useEffect(() => {
    fetchProjects();
  }, [fetchProjects]);

  const handleCreate = async (data: { name: string; description: string }) => {
    if (!user) return;

    const response = await fetch("/api/projects", {
      method: "POST",
      headers: { "Content-Type": "application/json", "X-User-Id": user.id },
      body: JSON.stringify(data),
    });

    if (!response.ok) {
      const err = await response.json();
      throw new Error(err.error || "Failed to create project");
    }

    const { project } = await response.json();
    setProjects((prev) => [project, ...prev]);
    setShowCreateForm(false);
  };

  const handleDelete = async (id: string) => {
    if (!user) return;

    const response = await fetch(`/api/projects/${id}`, {
      method: "DELETE",
      headers: { "X-User-Id": user.id },
    });
    if (!response.ok) throw new Error("Failed to delete project");
    setProjects((prev) => prev.filter((p) => p.id !== id));
  };

  const handleToggleStatus = async (id: string, status: "active" | "archived") => {
    if (!user) return;

    const response = await fetch(`/api/projects/${id}`, {
      method: "PUT",
      headers: { "Content-Type": "application/json", "X-User-Id": user.id },
      body: JSON.stringify({ status }),
    });

    if (!response.ok) throw new Error("Failed to update project");
    const { project } = await response.json();
    setProjects((prev) => prev.map((p) => (p.id === id ? project : p)));
  };

  if (isLoading) {
    return (
      <div className="space-y-6">
        <div className="flex items-center justify-between">
          <div className="h-8 w-32 animate-pulse rounded bg-muted" />
          <div className="h-10 w-28 animate-pulse rounded bg-muted" />
        </div>
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
          {[1, 2, 3].map((i) => (
            <div key={i} className="h-48 animate-pulse rounded-lg bg-muted" />
          ))}
        </div>
      </div>
    );
  }

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-bold tracking-tight">Projects</h1>
          <p className="text-muted-foreground">Manage your projects</p>
        </div>
        {!showCreateForm && <Button onClick={() => setShowCreateForm(true)}>New Project</Button>}
      </div>

      {error && (
        <div className="rounded-md bg-destructive/10 p-3 text-sm text-destructive">{error}</div>
      )}

      {showCreateForm && (
        <div className="rounded-lg border bg-card p-6">
          <h2 className="mb-4 text-lg font-semibold">Create Project</h2>
          <ProjectForm onSubmit={handleCreate} onCancel={() => setShowCreateForm(false)} />
        </div>
      )}

      {projects.length === 0 ? (
        <EmptyState
          icon={
            <svg
              className="h-12 w-12"
              fill="none"
              stroke="currentColor"
              viewBox="0 0 24 24"
              aria-hidden="true"
            >
              <path
                strokeLinecap="round"
                strokeLinejoin="round"
                strokeWidth={1.5}
                d="M3 7v10a2 2 0 002 2h14a2 2 0 002-2V9a2 2 0 00-2-2h-6l-2-2H5a2 2 0 00-2 2z"
              />
            </svg>
          }
          title="No projects yet"
          description="Create your first project to get started"
          action={{
            label: "Create Project",
            onClick: () => setShowCreateForm(true),
          }}
        />
      ) : (
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
          {projects.map((project) => (
            <ProjectCard
              key={project.id}
              project={project}
              onDelete={handleDelete}
              onToggleStatus={handleToggleStatus}
            />
          ))}
        </div>
      )}
    </div>
  );
}
