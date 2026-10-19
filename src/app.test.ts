import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { createServer, type Server } from 'http';
import type { AddressInfo } from 'net';
import type { Express } from 'express';
import { createApp } from './index';
import { MintRateLimiter } from './rateLimiter';
import { MemoryMintStore } from './mintStore';
import type { AppConfig } from './config';

const config: AppConfig = {
  environment: 'testnet',
  port: 0,
  adminSecret: 'test-secret',
  redisUrl: 'redis://127.0.0.1:6379',
  rateLimit: { maxMints: 3, windowMs: 60_000 },
  stellar: {
    network: 'testnet',
    rpcUrl: 'https://soroban-testnet.stellar.org',
    networkPassphrase: 'Test SDF Network ; September 2015',
  },
  contracts: {
    huntyCoreId: 'CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA',
    rewardManagerId: 'CBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB',
    nftRewardId: 'CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC',
  },
};

// G + 55 base32 chars — matches the /mint address regex only.
const VALID_ADDRESS = 'GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF';

async function listen(app: Express): Promise<{ server: Server; baseUrl: string }> {
  const server = createServer(app);
  await new Promise<void>((resolve) => {
    server.listen(0, '127.0.0.1', resolve);
  });
  const { port } = server.address() as AddressInfo;
  return { server, baseUrl: `http://127.0.0.1:${port}` };
}

describe('createApp', () => {
  let limiter: MintRateLimiter;
  let server: Server | undefined;

  beforeEach(() => {
    limiter = new MintRateLimiter(config.rateLimit, config.adminSecret, new MemoryMintStore());
  });

  afterEach(async () => {
    if (server) {
      await new Promise<void>((resolve, reject) => {
        server!.close((err) => (err ? reject(err) : resolve()));
      });
      server = undefined;
    }
  });

  it('can be imported and exercised without Redis or auto-listen side effects', async () => {
    const app = createApp({ config, limiter });
    const listening = await listen(app);
    server = listening.server;

    const res = await fetch(`${listening.baseUrl}/health`);
    expect(res.status).toBe(200);
    const body = (await res.json()) as { status: string; environment: string };
    expect(body.status).toBe('ok');
    expect(body.environment).toBe('testnet');
  });

  it('reserves a rate-limit slot without claiming that an NFT was minted', async () => {
    const app = createApp({ config, limiter });
    const listening = await listen(app);
    server = listening.server;

    const res = await fetch(`${listening.baseUrl}/mint/reserve`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ address: VALID_ADDRESS }),
    });
    expect(res.status).toBe(200);
    const body = (await res.json()) as {
      reserved: boolean;
      minted?: boolean;
      reservationsInWindow: number;
    };
    expect(body).toEqual({ reserved: true, reservationsInWindow: 1 });
    expect(body.minted).toBeUndefined();
    expect(body.reservationsInWindow).toBe(1);

    const oldEndpoint = await fetch(`${listening.baseUrl}/mint`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ address: VALID_ADDRESS }),
    });
    expect(oldEndpoint.status).toBe(404);
  });

  it('keys the global limiter on the forwarded client IP when trustProxy is set', async () => {
    const app = createApp({ config: { ...config, trustProxy: 1 }, limiter });
    const listening = await listen(app);
    server = listening.server;

    const post = (ip: string) =>
      fetch(`${listening.baseUrl}/mint/reserve`, {
        method: 'POST',
        headers: { 'content-type': 'application/json', 'x-forwarded-for': ip },
        body: JSON.stringify({ address: 'invalid' }),
      });

    for (let i = 0; i < 100; i++) {
      expect((await post('203.0.113.1')).status).toBe(400);
    }
    expect((await post('203.0.113.1')).status).toBe(429);
    // A different client behind the same proxy is not affected.
    expect((await post('203.0.113.2')).status).toBe(400);
  });

  it('rate-limits repeated admin secret guesses', async () => {
    const app = createApp({ config, limiter });
    const listening = await listen(app);
    server = listening.server;

    for (let i = 0; i < 5; i++) {
      const res = await fetch(`${listening.baseUrl}/admin/config`, {
        headers: { 'x-admin-secret': 'wrong' },
      });
      expect(res.status).toBe(403);
    }
    const limited = await fetch(`${listening.baseUrl}/admin/config`, {
      headers: { 'x-admin-secret': 'wrong' },
    });
    expect(limited.status).toBe(429);
  });

  it('persists admin config updates and returns the shared config', async () => {
    const app = createApp({ config, limiter });
    const listening = await listen(app);
    server = listening.server;

    const patch = await fetch(`${listening.baseUrl}/admin/config`, {
      method: 'PATCH',
      headers: { 'content-type': 'application/json', 'x-admin-secret': config.adminSecret },
      body: JSON.stringify({ maxMints: 7 }),
    });
    expect(patch.status).toBe(200);
    expect(await patch.json()).toEqual({ maxMints: 7, windowMs: 60_000 });

    const get = await fetch(`${listening.baseUrl}/admin/config`, {
      headers: { 'x-admin-secret': config.adminSecret },
    });
    expect(get.status).toBe(200);
    expect((await get.json()).rateLimit).toEqual({ maxMints: 7, windowMs: 60_000 });
  });

  it('rejects invalid addresses on GET /mint/count/:address', async () => {
    const app = createApp({ config, limiter });
    const listening = await listen(app);
    server = listening.server;

    const res = await fetch(`${listening.baseUrl}/mint/count/not-an-address`);
    expect(res.status).toBe(400);

    const ok = await fetch(`${listening.baseUrl}/mint/count/${VALID_ADDRESS}`);
    expect(ok.status).toBe(200);
  });
});
