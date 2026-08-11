/**
 * `Response#json()` resolves to `unknown` (not `any`), so every call site
 * needs to assert the shape it expects. Centralizing that assertion here
 * keeps individual call sites terse while making the "trust the server"
 * boundary explicit in one place.
 */
export async function getJson<T>(res: Response): Promise<T> {
  return (await res.json()) as T;
}
