import { useCallback, useEffect, useState } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import { ConfirmDialog } from "@/components/ConfirmDialog";
import type { Project } from "@/components/ProjectCard";
import { ProjectForm } from "@/components/ProjectForm";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { useAuth } from "@/hooks/use-auth";

export function ProjectDetailPage() {
  const { id } = useParams<{ id: string }>();
  const navigate = useNavigate();
  const { user } = useAuth();
  const [project, setProject] = useState<Project | null>(null);
  const [isLoading, setIsLoading] = useState(true);
  const [isEditing, setIsEditing] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const fetchProject = useCallback(async () => {
    if (!id || !user) return;

    try {
      const response = await fetch(`/api/projects/${id}`, {
        headers: { "X-User-Id": user.id },
      });
      if (response.status === 404) {
        setError("Project not found");
        return;
      }
      if (!response.ok) throw new Error("Failed to fetch project");
      const data = await response.json();
      setProject(data.project);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to load project");
    } finally {
      setIsLoading(false);
    }
  }, [id, user]);

  useEffect(() => {
    fetchProject();
  }, [fetchProject]);

  const handleUpdate = async (data: { name: string; description: string }) => {
    if (!id || !user) return;

    const response = await fetch(`/api/projects/${id}`, {
      method: "PUT",
      headers: { "Content-Type": "application/json", "X-User-Id": user.id },
      body: JSON.stringify(data),
    });

    if (!response.ok) {
      const err = await response.json();
      throw new Error(err.error || "Failed to update project");
    }

    const { project: updated } = await response.json();
    setProject(updated);
    setIsEditing(false);
  };

  const handleDelete = async () => {
    if (!id || !user) return;

    const response = await fetch(`/api/projects/${id}`, {
      method: "DELETE",
      headers: { "X-User-Id": user.id },
    });
    if (!response.ok) throw new Error("Failed to delete project");
    navigate("/projects");
  };

  if (isLoading) {
    return (
      <div className="mx-auto max-w-2xl space-y-6">
        <div className="h-8 w-48 animate-pulse rounded bg-muted" />
        <div className="h-64 animate-pulse rounded-lg bg-muted" />
      </div>
    );
  }

  if (error) {
    return (
      <div className="mx-auto max-w-2xl space-y-6">
        <Card>
          <CardContent className="py-12 text-center">
            <h2 className="text-lg font-semibold">Error</h2>
            <p className="mt-2 text-muted-foreground">{error}</p>
            <Button className="mt-4" asChild>
              <Link to="/projects">Back to Projects</Link>
            </Button>
          </CardContent>
        </Card>
      </div>
    );
  }

  if (!project) return null;

  return (
    <div className="mx-auto max-w-2xl space-y-6">
      <div className="flex items-center gap-2 text-sm text-muted-foreground">
        <Link to="/projects" className="hover:text-foreground">
          Projects
        </Link>
        <span>/</span>
        <span>{project.name}</span>
      </div>

      {isEditing ? (
        <Card>
          <CardHeader>
            <CardTitle>Edit Project</CardTitle>
          </CardHeader>
          <CardContent>
            <ProjectForm
              project={project}
              onSubmit={handleUpdate}
              onCancel={() => setIsEditing(false)}
            />
          </CardContent>
        </Card>
      ) : (
        <Card>
          <CardHeader>
            <div className="flex items-start justify-between">
              <div>
                <CardTitle className="flex items-center gap-2">
                  {project.name}
                  <span
                    className={`inline-flex items-center rounded-full px-2 py-0.5 text-xs font-medium ${
                      project.status === "archived"
                        ? "bg-muted text-muted-foreground"
                        : "bg-green-100 text-green-800 dark:bg-green-900 dark:text-green-100"
                    }`}
                  >
                    {project.status}
                  </span>
                </CardTitle>
                <CardDescription>
                  Created {new Date(project.created_at).toLocaleDateString()}
                  {project.updated_at !== project.created_at && (
                    <> • Updated {new Date(project.updated_at).toLocaleDateString()}</>
                  )}
                </CardDescription>
              </div>
              <div className="flex gap-2">
                <Button variant="outline" size="sm" onClick={() => setIsEditing(true)}>
                  Edit
                </Button>
                <ConfirmDialog
                  trigger={
                    <Button variant="destructive" size="sm">
                      Delete
                    </Button>
                  }
                  title="Delete Project"
                  description={`Are you sure you want to delete "${project.name}"? This action cannot be undone.`}
                  confirmLabel="Delete"
                  variant="destructive"
                  onConfirm={handleDelete}
                />
              </div>
            </div>
          </CardHeader>
          <CardContent>
            <div className="space-y-4">
              <div>
                <h3 className="text-sm font-medium text-muted-foreground">Description</h3>
                <p className="mt-1">{project.description || "No description provided"}</p>
              </div>
              <div>
                <h3 className="text-sm font-medium text-muted-foreground">Project ID</h3>
                <code className="mt-1 text-xs">{project.id}</code>
              </div>
            </div>
          </CardContent>
        </Card>
      )}
    </div>
  );
}
