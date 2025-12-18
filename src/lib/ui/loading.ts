import { escapeHtml } from "./helpers";

/**
 * Render loading state during factory reset
 */
export function renderLoadingState(message: string = "Resetting workspace..."): void {
  const container = document.getElementById("primary-terminal");
  if (!container) return;

  container.innerHTML = `
    <div class="h-full flex items-center justify-center bg-gray-200 dark:bg-gray-900">
      <div class="flex flex-col items-center gap-6">
        <!-- Loom weaving animation -->
        <div class="relative w-32 h-32">
          <!-- Vertical warp threads -->
          <div class="absolute inset-0 flex justify-around items-center">
            <div class="w-1 h-full bg-blue-400 dark:bg-blue-600 opacity-60 animate-pulse" style="animation-delay: 0ms; animation-duration: 1.5s;"></div>
            <div class="w-1 h-full bg-blue-400 dark:bg-blue-600 opacity-60 animate-pulse" style="animation-delay: 200ms; animation-duration: 1.5s;"></div>
            <div class="w-1 h-full bg-blue-400 dark:bg-blue-600 opacity-60 animate-pulse" style="animation-delay: 400ms; animation-duration: 1.5s;"></div>
            <div class="w-1 h-full bg-blue-400 dark:bg-blue-600 opacity-60 animate-pulse" style="animation-delay: 600ms; animation-duration: 1.5s;"></div>
            <div class="w-1 h-full bg-blue-400 dark:bg-blue-600 opacity-60 animate-pulse" style="animation-delay: 800ms; animation-duration: 1.5s;"></div>
          </div>

          <!-- Horizontal weft shuttle -->
          <div class="absolute inset-0 flex items-center overflow-hidden">
            <div class="w-full h-1 bg-gradient-to-r from-transparent via-blue-500 dark:via-blue-400 to-transparent animate-pulse" style="animation-duration: 2s;"></div>
          </div>

          <!-- Weaving shuttle moving across and up -->
          <div class="absolute inset-0 flex items-center">
            <div class="h-2 w-6 bg-blue-600 dark:bg-blue-400 rounded-full shadow-lg" style="animation: shuttle 4s ease-in-out infinite;"></div>
          </div>
        </div>

        <!-- Animated message -->
        <div class="text-center">
          <p class="text-lg font-semibold text-gray-700 dark:text-gray-300 animate-pulse">${escapeHtml(message)}</p>
          <p class="text-sm text-gray-500 dark:text-gray-400 mt-2">This may take a few moments...</p>
        </div>
      </div>
    </div>

    <style>
      @keyframes shuttle {
        0% {
          transform: translateX(-200%) translateY(50%);
        }
        22% {
          transform: translateX(600%) translateY(50%);
        }
        25% {
          transform: translateX(600%) translateY(30%);
        }
        47% {
          transform: translateX(-200%) translateY(30%);
        }
        50% {
          transform: translateX(-200%) translateY(10%);
        }
        72% {
          transform: translateX(600%) translateY(10%);
        }
        75% {
          transform: translateX(600%) translateY(-10%);
        }
        97% {
          transform: translateX(-200%) translateY(-10%);
        }
        100% {
          transform: translateX(-200%) translateY(50%);
        }
      }
    </style>
  `;
}
