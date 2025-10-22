import { open } from "@tauri-apps/api/dialog";
import { homeDir } from "@tauri-apps/api/path";
import { invoke } from "@tauri-apps/api/tauri";
import { Logger } from "./logger";

const logger = Logger.forComponent("create-project-modal");

export async function showCreateProjectModal(
  onProjectCreated: (projectPath: string) => void
): Promise<void> {
  // Create modal backdrop
  const backdrop = document.createElement("div");
  backdrop.className =
    "fixed inset-0 bg-black bg-opacity-50 flex items-center justify-center z-50 animate-fade-in";
  backdrop.id = "create-project-modal-backdrop";

  // Create modal content
  const modal = document.createElement("div");
  modal.className =
    "bg-white dark:bg-gray-800 rounded-lg shadow-xl w-full max-w-md mx-4 animate-scale-in";
  modal.setAttribute("role", "dialog");
  modal.setAttribute("aria-modal", "true");
  modal.setAttribute("aria-labelledby", "create-project-title");

  // Get default location
  const defaultLocation = await homeDir();

  modal.innerHTML = `
    <div class="px-6 py-4 border-b border-gray-200 dark:border-gray-700">
      <h2 id="create-project-title" class="text-xl font-semibold text-gray-900 dark:text-gray-100">Create New Project</h2>
    </div>
    <form id="create-project-form" class="px-6 py-4 space-y-4">
      <!-- Project Name -->
      <div>
        <label for="project-name" class="block text-sm font-medium text-gray-700 dark:text-gray-300 mb-1">
          Project Name <span class="text-red-500">*</span>
        </label>
        <input
          id="project-name"
          type="text"
          required
          placeholder="my-awesome-project"
          class="w-full px-3 py-2 text-sm bg-white dark:bg-gray-900 border border-gray-300 dark:border-gray-600 rounded focus:outline-none focus:ring-2 focus:ring-blue-500"
        />
        <p id="project-name-error" class="text-xs text-red-500 dark:text-red-400 mt-1 min-h-[16px]"></p>
      </div>

      <!-- Location -->
      <div>
        <label for="project-location" class="block text-sm font-medium text-gray-700 dark:text-gray-300 mb-1">
          Location <span class="text-red-500">*</span>
        </label>
        <div class="flex gap-2">
          <input
            id="project-location"
            type="text"
            required
            value="${defaultLocation}"
            class="flex-1 px-3 py-2 text-sm bg-white dark:bg-gray-900 border border-gray-300 dark:border-gray-600 rounded focus:outline-none focus:ring-2 focus:ring-blue-500"
          />
          <button
            type="button"
            id="browse-location-btn"
            class="px-3 py-2 text-sm bg-gray-100 dark:bg-gray-700 hover:bg-gray-200 dark:hover:bg-gray-600 border border-gray-300 dark:border-gray-600 rounded transition-colors"
            title="Browse for folder"
          >
            📁
          </button>
        </div>
      </div>

      <!-- Description -->
      <div>
        <label for="project-description" class="block text-sm font-medium text-gray-700 dark:text-gray-300 mb-1">
          Description (optional)
        </label>
        <textarea
          id="project-description"
          rows="2"
          placeholder="A brief description of your project"
          class="w-full px-3 py-2 text-sm bg-white dark:bg-gray-900 border border-gray-300 dark:border-gray-600 rounded focus:outline-none focus:ring-2 focus:ring-blue-500 resize-none"
        ></textarea>
      </div>

      <!-- License -->
      <div>
        <label for="project-license" class="block text-sm font-medium text-gray-700 dark:text-gray-300 mb-1">
          License
        </label>
        <select
          id="project-license"
          class="w-full px-3 py-2 text-sm bg-white dark:bg-gray-900 border border-gray-300 dark:border-gray-600 rounded focus:outline-none focus:ring-2 focus:ring-blue-500"
        >
          <option value="">None</option>
          <option value="MIT">MIT</option>
          <option value="Apache-2.0">Apache 2.0</option>
        </select>
      </div>

      <!-- GitHub Integration -->
      <div class="border-t border-gray-200 dark:border-gray-700 pt-4">
        <div class="flex items-center gap-2 mb-3">
          <input
            id="create-github-repo"
            type="checkbox"
            class="w-4 h-4 text-blue-600 bg-white dark:bg-gray-900 border-gray-300 dark:border-gray-600 rounded focus:ring-2 focus:ring-blue-500"
          />
          <label for="create-github-repo" class="text-sm font-medium text-gray-700 dark:text-gray-300">
            Create GitHub repository
          </label>
        </div>

        <div id="github-options" class="ml-6 space-y-3 hidden">
          <!-- Visibility -->
          <div>
            <label class="block text-sm font-medium text-gray-700 dark:text-gray-300 mb-1">
              Visibility
            </label>
            <div class="flex gap-4">
              <label class="flex items-center gap-2">
                <input
                  type="radio"
                  name="github-visibility"
                  value="public"
                  checked
                  class="w-4 h-4 text-blue-600 bg-white dark:bg-gray-900 border-gray-300 dark:border-gray-600 focus:ring-2 focus:ring-blue-500"
                />
                <span class="text-sm text-gray-700 dark:text-gray-300">Public</span>
              </label>
              <label class="flex items-center gap-2">
                <input
                  type="radio"
                  name="github-visibility"
                  value="private"
                  class="w-4 h-4 text-blue-600 bg-white dark:bg-gray-900 border-gray-300 dark:border-gray-600 focus:ring-2 focus:ring-blue-500"
                />
                <span class="text-sm text-gray-700 dark:text-gray-300">Private</span>
              </label>
            </div>
          </div>
        </div>
      </div>

      <!-- Error Message -->
      <div id="create-project-error" class="text-sm text-red-500 dark:text-red-400 min-h-[20px]"></div>
    </form>
    <div class="px-6 py-4 border-t border-gray-200 dark:border-gray-700 flex justify-end gap-2">
      <button
        type="button"
        id="cancel-create-project-btn"
        class="px-4 py-2 text-sm font-medium text-gray-700 dark:text-gray-300 bg-gray-100 dark:bg-gray-700 hover:bg-gray-200 dark:hover:bg-gray-600 rounded transition-colors"
      >
        Cancel
      </button>
      <button
        type="submit"
        form="create-project-form"
        id="create-project-btn"
        class="px-4 py-2 text-sm font-medium text-white bg-blue-600 hover:bg-blue-700 rounded transition-colors"
      >
        Create
      </button>
    </div>
  `;

  backdrop.appendChild(modal);
  document.body.appendChild(backdrop);

  // Get form elements
  const form = document.getElementById("create-project-form") as HTMLFormElement;
  const nameInput = document.getElementById("project-name") as HTMLInputElement;
  const locationInput = document.getElementById("project-location") as HTMLInputElement;
  const descriptionInput = document.getElementById("project-description") as HTMLTextAreaElement;
  const licenseSelect = document.getElementById("project-license") as HTMLSelectElement;
  const createGithubCheckbox = document.getElementById("create-github-repo") as HTMLInputElement;
  const githubOptions = document.getElementById("github-options");
  const browseBtn = document.getElementById("browse-location-btn");
  const cancelBtn = document.getElementById("cancel-create-project-btn");
  const errorDiv = document.getElementById("create-project-error");
  const nameErrorDiv = document.getElementById("project-name-error");

  // Validate project name
  function validateProjectName(name: string): string | null {
    if (!name.trim()) {
      return "Project name is required";
    }
    // Check for invalid characters
    if (!/^[a-zA-Z0-9-_]+$/.test(name)) {
      return "Project name can only contain letters, numbers, hyphens, and underscores";
    }
    return null;
  }

  // Browse for location
  browseBtn?.addEventListener("click", async () => {
    try {
      const selected = await open({
        directory: true,
        multiple: false,
        title: "Select project location",
      });

      if (selected && typeof selected === "string") {
        locationInput.value = selected;
      }
    } catch (error) {
      logger.error("Error browsing for location", error as Error);
    }
  });

  // Toggle GitHub options visibility
  createGithubCheckbox.addEventListener("change", () => {
    if (githubOptions) {
      githubOptions.classList.toggle("hidden", !createGithubCheckbox.checked);
    }
  });

  // Real-time validation
  nameInput.addEventListener("input", () => {
    const error = validateProjectName(nameInput.value);
    if (nameErrorDiv) {
      nameErrorDiv.textContent = error || "";
    }
  });

  // Handle form submission
  form.addEventListener("submit", async (e) => {
    e.preventDefault();

    const name = nameInput.value.trim();
    const location = locationInput.value.trim();
    const description = descriptionInput.value.trim() || null;
    const license = licenseSelect.value || null;
    const createGithub = createGithubCheckbox.checked;
    const githubVisibility = createGithub
      ? (document.querySelector('input[name="github-visibility"]:checked') as HTMLInputElement)
          ?.value || "public"
      : null;

    // Validate
    const nameError = validateProjectName(name);
    if (nameError) {
      if (nameErrorDiv) nameErrorDiv.textContent = nameError;
      return;
    }

    if (!location) {
      if (errorDiv) errorDiv.textContent = "Location is required";
      return;
    }

    // Clear errors
    if (errorDiv) errorDiv.textContent = "";
    if (nameErrorDiv) nameErrorDiv.textContent = "";

    try {
      // Disable form while creating
      const createBtn = document.getElementById("create-project-btn");
      if (createBtn) {
        createBtn.textContent = createGithub ? "Creating (local + GitHub)..." : "Creating...";
        (createBtn as HTMLButtonElement).disabled = true;
      }

      // Call backend to create project
      const projectPath = await invoke<string>("create_local_project", {
        name,
        location,
        description,
        license,
      });

      logger.info("Project created", { projectPath });

      // If GitHub checkbox is selected, create GitHub repository
      if (createGithub) {
        try {
          await invoke<string>("create_github_repository", {
            projectPath,
            name,
            description,
            isPrivate: githubVisibility === "private",
          });
          logger.info("GitHub repository created", { projectPath, name });
        } catch (githubError) {
          logger.error("Failed to create GitHub repository", githubError as Error, {
            projectPath,
            name,
          });
          if (errorDiv) {
            errorDiv.textContent = `Local project created, but GitHub creation failed: ${
              typeof githubError === "string" ? githubError : "Unknown error"
            }`;
          }
          // Re-enable button but keep modal open for user to see error
          const createBtn = document.getElementById("create-project-btn");
          if (createBtn) {
            createBtn.textContent = "Create";
            (createBtn as HTMLButtonElement).disabled = false;
          }
          return;
        }
      }

      // Close modal
      backdrop.remove();

      // Notify parent
      onProjectCreated(projectPath);
    } catch (error) {
      logger.error("Failed to create project", error as Error, {
        name,
        location,
      });
      if (errorDiv) {
        errorDiv.textContent =
          typeof error === "string" ? error : "Failed to create project. Please try again.";
      }

      // Re-enable button
      const createBtn = document.getElementById("create-project-btn");
      if (createBtn) {
        createBtn.textContent = "Create";
        (createBtn as HTMLButtonElement).disabled = false;
      }
    }
  });

  // Handle cancel
  cancelBtn?.addEventListener("click", () => {
    backdrop.remove();
  });

  // Close on backdrop click
  backdrop.addEventListener("click", (e) => {
    if (e.target === backdrop) {
      backdrop.remove();
    }
  });

  // Close on Escape key
  const handleEscape = (e: KeyboardEvent) => {
    if (e.key === "Escape") {
      backdrop.remove();
      document.removeEventListener("keydown", handleEscape);
    }
  };
  document.addEventListener("keydown", handleEscape);

  // Focus name input
  nameInput.focus();
}
