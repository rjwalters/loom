# Common Tasks

## Adding a New Agent Terminal Property

1. Update interface in `src/lib/state.ts`:
   ```typescript
   export interface Terminal {
     id: string;
     name: string;
     status: TerminalStatus;
     isPrimary: boolean;
     workingDirectory?: string; // NEW
   }
   ```

2. Update UI rendering in `src/lib/ui.ts`:
   ```typescript
   // Display new property
   <span>${escapeHtml(terminal.workingDirectory || 'N/A')}</span>
   ```

3. TypeScript will catch any missing properties at compile time

## Adding a New State Method

1. Add method to `AppState` class in `src/lib/state.ts`
2. Call `this.notify()` after state changes
3. UI will automatically re-render

## Adding a New UI Section

1. Add HTML structure to `index.html`
2. Create render function in `src/lib/ui.ts`
3. Call from `render()` in `src/main.ts`
4. Add event listeners in `setupEventListeners()`

## Adding a New Tauri Command

1. **Add Rust command** in `src-tauri/src/main.rs`:
   ```rust
   #[tauri::command]
   fn my_command(param: String) -> Result<ReturnType, String> {
       // Implementation
       Ok(result)
   }
   ```

2. **Register command** in `main()`:
   ```rust
   tauri::Builder::default()
       .invoke_handler(tauri::generate_handler![my_command])
   ```

3. **Call from TypeScript** in `src/main.ts`:
   ```typescript
   import { invoke } from '@tauri-apps/api/tauri';

   const result = await invoke<ReturnType>('my_command', { param: value });
   ```

4. **Add required APIs** to `src-tauri/tauri.conf.json` allowlist if needed
5. **Update Cargo.toml** if new Tauri features required
