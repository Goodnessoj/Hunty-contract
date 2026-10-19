import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { MintRateLimiter } from './rateLimiter';
import { MintRateLimitError } from './errors';
import { MemoryMintStore } from './mintStore';

describe('MintRateLimiter', () => {
  let limiter: MintRateLimiter;

  beforeEach(() => {
    vi.useFakeTimers();
    limiter = new MintRateLimiter(
      { maxMints: 3, windowMs: 60_000 },
      'secret',
      new MemoryMintStore(),
    );
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  describe('mint', () => {
    it('allows minting within the limit', async () => {
      await limiter.mint('addr1');
      expect(await limiter.getMintCount('addr1')).toBe(1);
    });

    it('blocks minting when limit is exceeded', async () => {
      await limiter.mint('addr1');
      await limiter.mint('addr1');
      await limiter.mint('addr1');
      await expect(limiter.mint('addr1')).rejects.toThrow(MintRateLimitError);
    });

    it('returns cooldown time in the error', async () => {
      await limiter.mint('addr1');
      await limiter.mint('addr1');
      await limiter.mint('addr1');
      try {
        await limiter.mint('addr1');
        expect.fail('expected MintRateLimitError');
      } catch (err) {
        expect(err).toBeInstanceOf(MintRateLimitError);
        const typed = err as MintRateLimitError;
        expect(typed.cooldownMs).toBeGreaterThan(0);
        expect(typed.cooldownMs).toBeLessThanOrEqual(60_000);
        expect(typed.message).toContain('seconds');
      }
    });

    it('allows minting again after the window expires', async () => {
      await limiter.mint('addr1');
      await limiter.mint('addr1');
      await limiter.mint('addr1');
      await expect(limiter.mint('addr1')).rejects.toThrow(MintRateLimitError);
      vi.advanceTimersByTime(60_001);
      await expect(limiter.mint('addr1')).resolves.toBeUndefined();
      expect(await limiter.getMintCount('addr1')).toBe(1);
    });
  });

  describe('per-address tracking', () => {
    it('tracks mints independently per address', async () => {
      await limiter.mint('addr1');
      await limiter.mint('addr1');
      await limiter.mint('addr2');
      expect(await limiter.getMintCount('addr1')).toBe(2);
      expect(await limiter.getMintCount('addr2')).toBe(1);
    });

    it('allows different addresses to mint independently', async () => {
      await limiter.mint('addr1');
      await limiter.mint('addr1');
      await limiter.mint('addr1');
      await expect(limiter.mint('addr2')).resolves.toBeUndefined();
    });
  });

  describe('getMintCount', () => {
    it('returns 0 for addresses with no mints', async () => {
      expect(await limiter.getMintCount('unknown')).toBe(0);
    });

    it('only counts mints within the current window', async () => {
      await limiter.mint('addr1');
      vi.advanceTimersByTime(30_000);
      await limiter.mint('addr1');
      expect(await limiter.getMintCount('addr1')).toBe(2);
      vi.advanceTimersByTime(31_000);
      expect(await limiter.getMintCount('addr1')).toBe(1);
    });
  });

  describe('shared store', () => {
    it('shares mint history across limiter instances using the same store', async () => {
      const shared = new MemoryMintStore();
      const a = new MintRateLimiter({ maxMints: 2, windowMs: 60_000 }, 'secret', shared);
      const b = new MintRateLimiter({ maxMints: 2, windowMs: 60_000 }, 'secret', shared);

      await a.mint('addr1');
      await a.mint('addr1');
      await expect(b.mint('addr1')).rejects.toThrow(MintRateLimitError);
      expect(await b.getMintCount('addr1')).toBe(2);
    });
  });

  describe('admin config', () => {
    it('returns current config', async () => {
      expect(await limiter.getConfig()).toEqual({ maxMints: 3, windowMs: 60_000 });
    });

    it('updates maxMints', async () => {
      await limiter.updateConfig({ maxMints: 5 }, 'secret');
      expect((await limiter.getConfig()).maxMints).toBe(5);
    });

    it('updates windowMs', async () => {
      await limiter.updateConfig({ windowMs: 120_000 }, 'secret');
      expect((await limiter.getConfig()).windowMs).toBe(120_000);
    });

    it('rejects unauthorized updates', async () => {
      await expect(limiter.updateConfig({ maxMints: 5 }, 'wrong-secret')).rejects.toThrow('Unauthorized');
    });

    it('rejects invalid maxMints', async () => {
      await expect(limiter.updateConfig({ maxMints: 0 }, 'secret')).rejects.toThrow('maxMints must be at least 1');
    });

    it('rejects invalid windowMs', async () => {
      await expect(limiter.updateConfig({ windowMs: 500 }, 'secret')).rejects.toThrow('windowMs must be at least 1000');
    });

    it('rejects non-integer, string and NaN values', async () => {
      await expect(limiter.updateConfig({ maxMints: '5' as unknown as number }, 'secret')).rejects.toThrow('maxMints');
      await expect(limiter.updateConfig({ maxMints: 2.5 }, 'secret')).rejects.toThrow('maxMints');
      await expect(limiter.updateConfig({ maxMints: NaN }, 'secret')).rejects.toThrow('maxMints');
      await expect(limiter.updateConfig({ windowMs: 'abc' as unknown as number }, 'secret')).rejects.toThrow('windowMs');
      await expect(limiter.updateConfig({ windowMs: 1.5 }, 'secret')).rejects.toThrow('windowMs');
      await expect(limiter.updateConfig({ windowMs: NaN }, 'secret')).rejects.toThrow('windowMs');
    });

    it('rejects values above the upper bound', async () => {
      await expect(limiter.updateConfig({ maxMints: 10_001 }, 'secret')).rejects.toThrow('maxMints');
      await expect(limiter.updateConfig({ windowMs: 86_400_001 }, 'secret')).rejects.toThrow('windowMs');
    });

    it('rejects unknown keys', async () => {
      await expect(
        limiter.updateConfig({ maxMints: 5, bogus: 1 } as unknown as { maxMints: number }, 'secret'),
      ).rejects.toThrow('Unknown config key');
    });

    it('applies updated limits immediately', async () => {
      await limiter.mint('addr1');
      await limiter.mint('addr1');
      await limiter.mint('addr1');
      await expect(limiter.mint('addr1')).rejects.toThrow(MintRateLimitError);
      await limiter.updateConfig({ maxMints: 5 }, 'secret');
      await expect(limiter.mint('addr1')).resolves.toBeUndefined();
    });

    it('shares updates across instances and preserves them after limiter recreation', async () => {
      const shared = new MemoryMintStore();
      const first = new MintRateLimiter({ maxMints: 2, windowMs: 60_000 }, 'secret', shared);
      const second = new MintRateLimiter({ maxMints: 3, windowMs: 120_000 }, 'secret', shared);

      await first.updateConfig({ maxMints: 5 }, 'secret');
      expect(await second.getConfig()).toEqual({ maxMints: 5, windowMs: 60_000 });

      const restarted = new MintRateLimiter({ maxMints: 3, windowMs: 120_000 }, 'secret', shared);
      expect(await restarted.getConfig()).toEqual({ maxMints: 5, windowMs: 60_000 });
    });
  });

  describe('memory cleanup', () => {
    it('removes stale records after window expires', async () => {
      await limiter.mint('addr1');
      await limiter.mint('addr2');
      await limiter.mint('addr3');
      expect(await limiter.getMintCount('addr1')).toBe(1);
      expect(await limiter.getMintCount('addr2')).toBe(1);
      expect(await limiter.getMintCount('addr3')).toBe(1);
      
      // Advance time past the window
      vi.advanceTimersByTime(60_001);
      
      // Trigger cleanup by minting a new address
      await limiter.mint('addr4');
      
      // Old addresses should now return 0 (records were cleaned up)
      expect(await limiter.getMintCount('addr1')).toBe(0);
      expect(await limiter.getMintCount('addr2')).toBe(0);
      expect(await limiter.getMintCount('addr3')).toBe(0);
      expect(await limiter.getMintCount('addr4')).toBe(1);
    });

    it('cleans up records with some expired timestamps', async () => {
      await limiter.mint('addr1');
      vi.advanceTimersByTime(30_000);
      await limiter.mint('addr1');
      vi.advanceTimersByTime(31_000);
      
      // Trigger cleanup
      await limiter.mint('addr2');
      
      // addr1 should have only the recent timestamp
      expect(await limiter.getMintCount('addr1')).toBe(1);
    });
  });
});
