import * as Toast from "@radix-ui/react-toast";
import { createContext, useCallback, useContext, useState } from "react";
import { cn } from "@/lib/utils";

interface ToastData {
  id: string;
  title: string;
  description?: string;
  variant?: "default" | "destructive";
}

interface ToastContextValue {
  toast: (data: Omit<ToastData, "id">) => void;
}

const ToastContext = createContext<ToastContextValue | undefined>(undefined);

export function useToast() {
  const context = useContext(ToastContext);
  if (!context) {
    throw new Error("useToast must be used within a Toaster");
  }
  return context;
}

export function Toaster({ children }: { children?: React.ReactNode }) {
  const [toasts, setToasts] = useState<ToastData[]>([]);

  const toast = useCallback((data: Omit<ToastData, "id">) => {
    const id = crypto.randomUUID();
    setToasts((prev) => [...prev, { ...data, id }]);
  }, []);

  const removeToast = useCallback((id: string) => {
    setToasts((prev) => prev.filter((t) => t.id !== id));
  }, []);

  return (
    <ToastContext.Provider value={{ toast }}>
      {children}
      <Toast.Provider swipeDirection="right">
        {toasts.map((t) => (
          <Toast.Root
            key={t.id}
            className={cn(
              "group pointer-events-auto relative flex w-full items-center justify-between space-x-4 overflow-hidden rounded-md border p-6 pr-8 shadow-lg transition-all",
              t.variant === "destructive"
                ? "border-destructive bg-destructive text-destructive-foreground"
                : "border bg-background text-foreground",
            )}
            onOpenChange={(open) => {
              if (!open) removeToast(t.id);
            }}
          >
            <div className="grid gap-1">
              <Toast.Title className="text-sm font-semibold">{t.title}</Toast.Title>
              {t.description && (
                <Toast.Description className="text-sm opacity-90">
                  {t.description}
                </Toast.Description>
              )}
            </div>
            <Toast.Close className="absolute right-2 top-2 rounded-md p-1 text-foreground/50 opacity-0 transition-opacity hover:text-foreground focus:opacity-100 focus:outline-none focus:ring-2 group-hover:opacity-100">
              <span className="sr-only">Close</span>
              <svg className="h-4 w-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                <title>Close</title>
                <path
                  strokeLinecap="round"
                  strokeLinejoin="round"
                  strokeWidth={2}
                  d="M6 18L18 6M6 6l12 12"
                />
              </svg>
            </Toast.Close>
          </Toast.Root>
        ))}
        <Toast.Viewport className="fixed bottom-0 right-0 z-[100] flex max-h-screen w-full flex-col-reverse p-4 sm:max-w-[420px]" />
      </Toast.Provider>
    </ToastContext.Provider>
  );
}
