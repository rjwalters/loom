import { Link } from "react-router-dom";
import { ConfirmDialog } from "@/components/ConfirmDialog";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";

export interface Project {
  id: string;
  name: string;
  description: string | null;
  status: "active" | "archived";
  created_at: string;
  updated_at: string;
}

interface ProjectCardProps {
  project: Project;
  onDelete: (id: string) => Promise<void>;
  onToggleStatus: (id: string, status: "active" | "archived") => Promise<void>;
}

export function ProjectCard({ project, onDelete, onToggleStatus }: ProjectCardProps) {
  const isArchived = project.status === "archived";

  return (
    <Card className={isArchived ? "opacity-60" : ""}>
      <CardHeader>
        <div className="flex items-start justify-between">
          <div>
            <CardTitle className="flex items-center gap-2">
              <Link to={`/projects/${project.id}`} className="hover:underline">
                {project.name}
              </Link>
              <span
                className={`inline-flex items-center rounded-full px-2 py-0.5 text-xs font-medium ${
                  isArchived
                    ? "bg-muted text-muted-foreground"
                    : "bg-green-100 text-green-800 dark:bg-green-900 dark:text-green-100"
                }`}
              >
                {project.status}
              </span>
            </CardTitle>
            <CardDescription className="mt-1">
              Created {new Date(project.created_at).toLocaleDateString()}
            </CardDescription>
          </div>
        </div>
      </CardHeader>
      <CardContent>
        <p className="text-sm text-muted-foreground line-clamp-2">
          {project.description || "No description"}
        </p>
        <div className="mt-4 flex gap-2">
          <Button variant="outline" size="sm" asChild>
            <Link to={`/projects/${project.id}`}>View</Link>
          </Button>
          <Button
            variant="ghost"
            size="sm"
            onClick={() => onToggleStatus(project.id, isArchived ? "active" : "archived")}
          >
            {isArchived ? "Restore" : "Archive"}
          </Button>
          <ConfirmDialog
            trigger={
              <Button variant="ghost" size="sm" className="text-destructive hover:text-destructive">
                Delete
              </Button>
            }
            title="Delete Project"
            description={`Are you sure you want to delete "${project.name}"? This action cannot be undone.`}
            confirmLabel="Delete"
            variant="destructive"
            onConfirm={() => onDelete(project.id)}
          />
        </div>
      </CardContent>
    </Card>
  );
}
