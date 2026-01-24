import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { useAuth } from "@/hooks/use-auth";

export function ProfilePage() {
  const { user } = useAuth();
  const [isEditing, setIsEditing] = useState(false);
  const [name, setName] = useState(user?.name || "");
  const [isSaving, setIsSaving] = useState(false);
  const [message, setMessage] = useState<{ type: "success" | "error"; text: string } | null>(null);

  const handleSave = async () => {
    if (!name.trim()) return;

    setIsSaving(true);
    setMessage(null);

    try {
      // In a real app, this would call PUT /api/profile
      // For demo, we just simulate success
      await new Promise((resolve) => setTimeout(resolve, 500));

      setMessage({ type: "success", text: "Profile updated successfully" });
      setIsEditing(false);
    } catch {
      setMessage({ type: "error", text: "Failed to update profile" });
    } finally {
      setIsSaving(false);
    }
  };

  if (!user) return null;

  return (
    <div className="mx-auto max-w-2xl space-y-6">
      <div>
        <h1 className="text-2xl font-bold tracking-tight">Profile</h1>
        <p className="text-muted-foreground">Manage your account information</p>
      </div>

      {message && (
        <div
          className={`rounded-md p-3 text-sm ${
            message.type === "success"
              ? "bg-green-100 text-green-800 dark:bg-green-900 dark:text-green-100"
              : "bg-destructive/10 text-destructive"
          }`}
        >
          {message.text}
        </div>
      )}

      <Card>
        <CardHeader>
          <CardTitle>Personal Information</CardTitle>
          <CardDescription>Your profile details</CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          {/* Avatar placeholder */}
          <div className="flex items-center gap-4">
            <div className="flex h-16 w-16 items-center justify-center rounded-full bg-primary text-2xl font-semibold text-primary-foreground">
              {user.name.charAt(0).toUpperCase()}
            </div>
            <div>
              <p className="text-sm text-muted-foreground">Avatar</p>
              <p className="text-xs text-muted-foreground">
                To add avatar support, integrate with a storage service
              </p>
            </div>
          </div>

          {/* Name field */}
          <div className="space-y-2">
            <label htmlFor="name" className="text-sm font-medium">
              Display Name
            </label>
            {isEditing ? (
              <div className="flex gap-2">
                <Input
                  id="name"
                  value={name}
                  onChange={(e) => setName(e.target.value)}
                  placeholder="Your name"
                />
                <Button onClick={handleSave} disabled={isSaving}>
                  {isSaving ? "Saving..." : "Save"}
                </Button>
                <Button
                  variant="outline"
                  onClick={() => {
                    setName(user.name);
                    setIsEditing(false);
                  }}
                  disabled={isSaving}
                >
                  Cancel
                </Button>
              </div>
            ) : (
              <div className="flex items-center justify-between rounded-md border px-3 py-2">
                <span>{user.name}</span>
                <Button variant="ghost" size="sm" onClick={() => setIsEditing(true)}>
                  Edit
                </Button>
              </div>
            )}
          </div>

          {/* Email field (read-only) */}
          <div className="space-y-2">
            <p className="text-sm font-medium">Email</p>
            <div className="flex items-center justify-between rounded-md border bg-muted/50 px-3 py-2">
              <span>{user.email}</span>
              <span className="text-xs text-muted-foreground">Read only</span>
            </div>
          </div>

          {/* Account info */}
          <div className="space-y-2">
            <p className="text-sm font-medium">Account ID</p>
            <div className="rounded-md border bg-muted/50 px-3 py-2">
              <code className="text-xs">{user.id}</code>
            </div>
          </div>
        </CardContent>
      </Card>
    </div>
  );
}
