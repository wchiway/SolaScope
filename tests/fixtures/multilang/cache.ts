export class TokenCache {
  private readonly entries = new Map<string, { value: string; expiresAt: number }>();

  storeSession(key: string, value: string, expiresAt: number): void {
    this.entries.set(key, { value, expiresAt });
  }

  readSession(key: string, now: number): string {
    // Read a cached session and reject expired tokens.
    const entry = this.entries.get(key);
    if (!entry || entry.expiresAt <= now) {
      throw new Error("TOKEN_CACHE_MISS");
    }
    return entry.value;
  }
}

export const formatUserLabel = (username: string): string => `User: ${username}`;
