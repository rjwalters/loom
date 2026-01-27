import { createContext, useCallback, useContext, useEffect, useState } from "react";

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

interface ApiError {
  error: string;
}

const AuthContext = createContext<AuthContextValue | undefined>(undefined);

async function fetchWithCredentials(url: string, options: RequestInit = {}): Promise<Response> {
  return fetch(url, {
    ...options,
    credentials: "include", // Include cookies for session management
    headers: {
      "Content-Type": "application/json",
      ...options.headers,
    },
  });
}

export function AuthProvider({ children }: { children: React.ReactNode }) {
  const [user, setUser] = useState<User | null>(null);
  const [isLoading, setIsLoading] = useState(true);

  // Check for existing session on mount
  useEffect(() => {
    const checkSession = async () => {
      try {
        const response = await fetchWithCredentials("/api/auth/me");
        if (response.ok) {
          const data = await response.json();
          setUser(data.user);
        } else {
          // Session expired or invalid - clear user state
          setUser(null);
        }
      } catch {
        // Network error or server unavailable
        setUser(null);
      } finally {
        setIsLoading(false);
      }
    };

    checkSession();
  }, []);

  const login = useCallback(async (email: string, password: string) => {
    setIsLoading(true);
    try {
      const response = await fetchWithCredentials("/api/auth/login", {
        method: "POST",
        body: JSON.stringify({ email, password }),
      });

      if (!response.ok) {
        const data: ApiError = await response.json();
        throw new Error(data.error || "Login failed");
      }

      const data = await response.json();
      setUser(data.user);
    } finally {
      setIsLoading(false);
    }
  }, []);

  const logout = useCallback(async () => {
    setIsLoading(true);
    try {
      await fetchWithCredentials("/api/auth/logout", {
        method: "POST",
      });
      setUser(null);
    } finally {
      setIsLoading(false);
    }
  }, []);

  const register = useCallback(async (email: string, password: string, name: string) => {
    setIsLoading(true);
    try {
      const response = await fetchWithCredentials("/api/auth/register", {
        method: "POST",
        body: JSON.stringify({ email, password, name }),
      });

      if (!response.ok) {
        const data: ApiError = await response.json();
        throw new Error(data.error || "Registration failed");
      }

      const data = await response.json();
      setUser(data.user);
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
