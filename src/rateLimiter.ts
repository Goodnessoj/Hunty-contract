import { RateLimitConfig, RateLimitConfigUpdate } from './types';
import { MintRateLimitError } from './errors';
import { MemoryMintStore, MintRecordStore } from './mintStore';

export class MintRateLimiter {
  private store: MintRecordStore;
  private defaultConfig: RateLimitConfig;
  private adminSecret: string;

  constructor(
    config: RateLimitConfig,
    adminSecret: string,
    store: MintRecordStore = new MemoryMintStore(),
  ) {
    this.defaultConfig = { ...config };
    this.adminSecret = adminSecret;
    this.store = store;
  }

  async check(address: string): Promise<void> {
    const config = await this.getConfig();
    const now = Date.now();
    const cutoff = now - config.windowMs;
    const recentMints = await this.store.getRecent(address, cutoff);

    if (recentMints.length >= config.maxMints) {
      const oldestRecent = Math.min(...recentMints);
      const cooldownMs = oldestRecent + config.windowMs - now;
      throw new MintRateLimitError(cooldownMs);
    }
  }

  async recordMint(address: string): Promise<void> {
    const config = await this.getConfig();
    const now = Date.now();
    const cutoff = now - config.windowMs;
    const result = await this.store.tryRecord(
      address,
      now,
      cutoff,
      config.maxMints,
    );
    if (!result.allowed) {
      const cooldownMs = result.oldestRecent + config.windowMs - now;
      throw new MintRateLimitError(cooldownMs);
    }
  }

  async mint(address: string): Promise<void> {
    const config = await this.getConfig();
    const now = Date.now();
    const cutoff = now - config.windowMs;
    const result = await this.store.tryRecord(
      address,
      now,
      cutoff,
      config.maxMints,
    );
    if (!result.allowed) {
      const cooldownMs = result.oldestRecent + config.windowMs - now;
      throw new MintRateLimitError(cooldownMs);
    }
  }

  async getMintCount(address: string): Promise<number> {
    const now = Date.now();
    const config = await this.getConfig();
    const cutoff = now - config.windowMs;
    const recent = await this.store.getRecent(address, cutoff);
    return recent.length;
  }

  async getConfig(): Promise<RateLimitConfig> {
    return this.store.initializeRateLimitConfig(this.defaultConfig);
  }

  async updateConfig(update: RateLimitConfigUpdate, secret: string): Promise<RateLimitConfig> {
    if (secret !== this.adminSecret) {
      throw new Error('Unauthorized: invalid admin secret');
    }
    if (update === null || typeof update !== 'object' || Array.isArray(update)) {
      throw new Error('Invalid config update');
    }
    const allowedKeys = new Set(['maxMints', 'windowMs']);
    for (const key of Object.keys(update)) {
      if (!allowedKeys.has(key)) {
        throw new Error(`Unknown config key: ${key}`);
      }
    }
    if (update.maxMints !== undefined) {
      if (!Number.isInteger(update.maxMints) || update.maxMints < 1 || update.maxMints > 10_000) {
        throw new Error('maxMints must be at least 1 (integer, max 10000)');
      }
    }
    if (update.windowMs !== undefined) {
      if (!Number.isInteger(update.windowMs) || update.windowMs < 1000 || update.windowMs > 86_400_000) {
        throw new Error('windowMs must be at least 1000 (integer, max 86400000)');
      }
    }
    await this.store.initializeRateLimitConfig(this.defaultConfig);
    return this.store.updateRateLimitConfig(update);
  }
}
