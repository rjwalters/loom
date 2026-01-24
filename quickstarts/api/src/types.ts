export interface Env {
  DB: D1Database;
  KV: KVNamespace;
  APP_NAME: string;
  JWT_SECRET: string;
  JWT_EXPIRES_IN: string;
}

export interface User {
  id: string;
  email: string;
  name: string;
  role: "user" | "admin";
  created_at: string;
  updated_at: string;
}

export interface JwtPayload {
  sub: string; // user id
  email: string;
  role: string;
  iat: number;
  exp: number;
}

export interface Session {
  userId: string;
  createdAt: number;
  expiresAt: number;
}
