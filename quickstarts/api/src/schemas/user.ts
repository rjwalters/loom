import { z } from "zod";

export const UserSchema = z.object({
  id: z.string(),
  email: z.string().email(),
  name: z.string(),
  role: z.enum(["user", "admin"]),
  created_at: z.string(),
  updated_at: z.string(),
});

export const UserListSchema = z.object({
  users: z.array(UserSchema),
  total: z.number(),
});

export const UpdateUserSchema = z.object({
  name: z.string().min(1).max(100).optional(),
  email: z.string().email().optional(),
});

export const UserParamsSchema = z.object({
  id: z.string().uuid("Invalid user ID"),
});

export type User = z.infer<typeof UserSchema>;
export type UserList = z.infer<typeof UserListSchema>;
export type UpdateUserInput = z.infer<typeof UpdateUserSchema>;
