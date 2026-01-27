/**
 * Hash password using PBKDF2 (edge-compatible)
 */
export async function hashPassword(
  password: string,
  salt?: string,
): Promise<{ hash: string; salt: string }> {
  const encoder = new TextEncoder();
  const passwordSalt =
    salt ||
    btoa(String.fromCharCode(...crypto.getRandomValues(new Uint8Array(16))));

  const keyMaterial = await crypto.subtle.importKey(
    "raw",
    encoder.encode(password),
    "PBKDF2",
    false,
    ["deriveBits"],
  );

  const derivedBits = await crypto.subtle.deriveBits(
    {
      name: "PBKDF2",
      salt: encoder.encode(passwordSalt),
      iterations: 100000,
      hash: "SHA-256",
    },
    keyMaterial,
    256,
  );

  const hash = btoa(String.fromCharCode(...new Uint8Array(derivedBits)));
  return { hash: `${passwordSalt}:${hash}`, salt: passwordSalt };
}

/**
 * Verify password against stored hash
 */
export async function verifyPassword(
  password: string,
  storedHash: string,
): Promise<boolean> {
  const [salt, expectedHash] = storedHash.split(":");
  if (!salt || !expectedHash) return false;

  const { hash } = await hashPassword(password, salt);
  const [, computedHash] = hash.split(":");

  // Constant-time comparison to prevent timing attacks
  if (computedHash.length !== expectedHash.length) return false;
  let result = 0;
  for (let i = 0; i < computedHash.length; i++) {
    result |= computedHash.charCodeAt(i) ^ expectedHash.charCodeAt(i);
  }
  return result === 0;
}
