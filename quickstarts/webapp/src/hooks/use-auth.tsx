import { createContext, useContext, useEffect, useState, useCallback } from "react";

interface User {
  id: string;
  email: string;
  name: string;
}

interface AuthContextValue {
  user: User | null;
  isLoading: boolean;
  login: (email: string, password: string) => Promise<void>;
  logout: () => Promise<void>;
  register: (email: string, password: string, name: string) => Promise<void>;
}

const AuthContext = createContext<AuthContextValue | undefined>(undefined);

const STORAGE_KEY = "loom-quickstart-auth";

export function AuthProvider({ children }: { children: React.ReactNode }) {
  const [user, setUser] = useState<User | null>(null);
  const [isLoading, setIsLoading] = useState(true);

  useEffect(() => {
    // Check for existing session
    const stored = localStorage.getItem(STORAGE_KEY);
    if (stored) {
      try {
        setUser(JSON.parse(stored));
      } catch {
        localStorage.removeItem(STORAGE_KEY);
      }
    }
    setIsLoading(false);
  }, []);

  const login = useCallback(async (email: string, password: string) => {
    // In a real app, this would call your authentication API
    // For demo purposes, we'll simulate a successful login
    setIsLoading(true);
    try {
      // Simulate API call
      await new Promise((resolve) => setTimeout(resolve, 500));

      // Demo: accept any email/password with basic validation
      if (!email || !password) {
        throw new Error("Email and password are required");
      }

      const newUser: User = {
        id: crypto.randomUUID(),
        email,
        name: email.split("@")[0],
      };

      localStorage.setItem(STORAGE_KEY, JSON.stringify(newUser));
      setUser(newUser);
    } finally {
      setIsLoading(false);
    }
  }, []);

  const logout = useCallback(async () => {
    setIsLoading(true);
    try {
      // Simulate API call
      await new Promise((resolve) => setTimeout(resolve, 200));
      localStorage.removeItem(STORAGE_KEY);
      setUser(null);
    } finally {
      setIsLoading(false);
    }
  }, []);

  const register = useCallback(async (email: string, password: string, name: string) => {
    setIsLoading(true);
    try {
      // Simulate API call
      await new Promise((resolve) => setTimeout(resolve, 500));

      if (!email || !password || !name) {
        throw new Error("All fields are required");
      }

      const newUser: User = {
        id: crypto.randomUUID(),
        email,
        name,
      };

      localStorage.setItem(STORAGE_KEY, JSON.stringify(newUser));
      setUser(newUser);
    } finally {
      setIsLoading(false);
    }
  }, []);

  return (
    <AuthContext.Provider value={{ user, isLoading, login, logout, register }}>
      {children}
    </AuthContext.Provider>
  );
}

export function useAuth() {
  const context = useContext(AuthContext);
  if (context === undefined) {
    throw new Error("useAuth must be used within an AuthProvider");
  }
  return context;
}
