import { createClient, RedisClientType } from 'redis';
import { randomUUID } from 'crypto';
import { RateLimitConfig, RateLimitConfigUpdate } from './types';

export type TryRecordResult =
  | { allowed: true; count: number }
  | { allowed: false; oldestRecent: number };

/**
 * Shared mint-history storage for the rate limiter.
 * Production must use Redis so limits survive restarts and stay correct
 * across multiple API instances behind a load balancer.
 */
export interface MintRecordStore {
  getRecent(address: string, cutoff: number): Promise<number[]>;
  /**
   * Atomically prune stale timestamps, enforce maxMints, and record `now`
   * when allowed. Implementations must make this race-safe for concurrent mints.
   */
  tryRecord(
    address: string,
    now: number,
    cutoff: number,
    maxMints: number,
  ): Promise<TryRecordResult>;
  getRateLimitConfig(): Promise<RateLimitConfig | null>;
  initializeRateLimitConfig(config: RateLimitConfig): Promise<RateLimitConfig>;
  updateRateLimitConfig(update: RateLimitConfigUpdate): Promise<RateLimitConfig>;
  close?(): Promise<void>;
}

/** In-memory store for unit tests only — not safe for multi-instance deploys. */
export class MemoryMintStore implements MintRecordStore {
  private records = new Map<string, number[]>();
  private rateLimitConfig: RateLimitConfig | null = null;

  async getRateLimitConfig(): Promise<RateLimitConfig | null> {
    return this.rateLimitConfig ? { ...this.rateLimitConfig } : null;
  }

  async initializeRateLimitConfig(config: RateLimitConfig): Promise<RateLimitConfig> {
    this.rateLimitConfig ??= { ...config };
    return { ...this.rateLimitConfig };
  }

  async updateRateLimitConfig(update: RateLimitConfigUpdate): Promise<RateLimitConfig> {
    if (!this.rateLimitConfig) {
      throw new Error('Rate limit config has not been initialized');
    }
    this.rateLimitConfig = { ...this.rateLimitConfig, ...update };
    return { ...this.rateLimitConfig };
  }

  async getRecent(address: string, cutoff: number): Promise<number[]> {
    const timestamps = this.records.get(address) ?? [];
    const recent = timestamps.filter(t => t > cutoff);
    this.records.set(address, recent);
    return [...recent];
  }

  async tryRecord(
    address: string,
    now: number,
    cutoff: number,
    maxMints: number,
  ): Promise<TryRecordResult> {
    const recent = (this.records.get(address) ?? []).filter(t => t > cutoff);
    if (recent.length >= maxMints) {
      return { allowed: false, oldestRecent: Math.min(...recent) };
    }
    recent.push(now);
    this.records.set(address, recent);
    return { allowed: true, count: recent.length };
  }
}

const KEY_PREFIX = 'mint:ratelimit:';
const CONFIG_KEY = `${KEY_PREFIX}config`;

export const UPDATE_CONFIG_LUA = `
local config = cjson.decode(redis.call('GET', KEYS[1]))
local update = cjson.decode(ARGV[1])
if update.maxMints ~= nil then config.maxMints = update.maxMints end
if update.windowMs ~= nil then config.windowMs = update.windowMs end
local encoded = cjson.encode(config)
redis.call('SET', KEYS[1], encoded)
return encoded
`;

function parseRateLimitConfig(value: string): RateLimitConfig {
  const parsed = JSON.parse(value) as Partial<RateLimitConfig>;
  if (!Number.isInteger(parsed.maxMints) || !Number.isInteger(parsed.windowMs)) {
    throw new Error('Stored rate limit config is invalid');
  }
  return { maxMints: parsed.maxMints!, windowMs: parsed.windowMs! };
}

export const TRY_RECORD_LUA = `
redis.call('ZREMRANGEBYSCORE', KEYS[1], '-inf', ARGV[1])
local count = redis.call('ZCARD', KEYS[1])
local maxMints = tonumber(ARGV[3])
if count >= maxMints then
  local oldest = redis.call('ZRANGE', KEYS[1], 0, 0, 'WITHSCORES')
  return {0, tonumber(oldest[2])}
end
redis.call('ZADD', KEYS[1], ARGV[2], ARGV[5])
redis.call('EXPIRE', KEYS[1], tonumber(ARGV[4]))
return {1, count + 1}
`;

export class RedisMintStore implements MintRecordStore {
  private client: RedisClientType;
  private connectPromise: Promise<void> | null = null;

  constructor(redisUrl: string) {
    this.client = createClient({ url: redisUrl });
    this.client.on('error', (err) => {
      console.error('Redis mint-store error:', err);
    });
  }

  private async ensureConnected(): Promise<void> {
    if (this.client.isOpen) {
      return;
    }
    if (!this.connectPromise) {
      this.connectPromise = this.client.connect().then(() => undefined);
    }
    await this.connectPromise;
  }

  private key(address: string): string {
    return `${KEY_PREFIX}${address}`;
  }

  async getRateLimitConfig(): Promise<RateLimitConfig | null> {
    await this.ensureConnected();
    const value = await this.client.get(CONFIG_KEY);
    return value === null ? null : parseRateLimitConfig(value);
  }

  async initializeRateLimitConfig(config: RateLimitConfig): Promise<RateLimitConfig> {
    await this.ensureConnected();
    await this.client.set(CONFIG_KEY, JSON.stringify(config), { NX: true });
    const stored = await this.client.get(CONFIG_KEY);
    if (stored === null) {
      throw new Error('Failed to initialize rate limit config');
    }
    return parseRateLimitConfig(stored);
  }

  async updateRateLimitConfig(update: RateLimitConfigUpdate): Promise<RateLimitConfig> {
    await this.ensureConnected();
    const value = await this.client.eval(UPDATE_CONFIG_LUA, {
      keys: [CONFIG_KEY],
      arguments: [JSON.stringify(update)],
    });
    if (typeof value !== 'string') {
      throw new Error('Rate limit config has not been initialized');
    }
    return parseRateLimitConfig(value);
  }

  async getRecent(address: string, cutoff: number): Promise<number[]> {
    await this.ensureConnected();
    const key = this.key(address);
    await this.client.zRemRangeByScore(key, '-inf', cutoff);
    const members = await this.client.zRangeByScore(key, cutoff, '+inf');
    return members.map((m) => {
      const sep = m.indexOf('-');
      // Members are stored as `${score}-${uuid}`; fall back to plain numbers
      // for entries written before the unique-member fix.
      return sep === -1 ? Number(m) : Number(m.slice(0, sep));
    });
  }

  async tryRecord(
    address: string,
    now: number,
    cutoff: number,
    maxMints: number,
  ): Promise<TryRecordResult> {
    await this.ensureConnected();
    // TTL covers a full window plus buffer so idle keys eventually expire.
    const ttlSeconds = Math.max(60, Math.ceil((now - cutoff) / 1000) * 2);
    const member = `${now}-${randomUUID()}`;
    const raw = (await this.client.eval(TRY_RECORD_LUA, {
      keys: [this.key(address)],
      arguments: [String(cutoff), String(now), String(maxMints), String(ttlSeconds), member],
    })) as Array<number | string>;

    const allowed = Number(raw[0]) === 1;
    const value = Number(raw[1]);
    if (allowed) {
      return { allowed: true, count: value };
    }
    return { allowed: false, oldestRecent: value };
  }

  async close(): Promise<void> {
    if (this.client.isOpen) {
      await this.client.quit();
    }
  }
}

export async function createMintStore(redisUrl: string): Promise<MintRecordStore> {
  const store = new RedisMintStore(redisUrl);
  // Fail fast at startup if Redis is unreachable
  await store.getRecent('__healthcheck__', 0);
  return store;
}
