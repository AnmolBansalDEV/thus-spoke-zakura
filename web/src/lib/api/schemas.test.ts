import { describe, expect, it } from 'vitest';
import { accountSchema, statusSchema } from './schemas';

const baseStatus = {
  instance: 'default',
  node: null,
  account_count: 5,
  auto_mine: true,
  network: 'Regtest',
};

describe('statusSchema', () => {
  it('accepts the server wallet synchronization status', () => {
    const status = statusSchema.parse({
      ...baseStatus,
      wallet_sync: {
        state: 'error',
        fully_scanned_height: 120,
        observed_height: 121,
        last_success_at: 1_789_700_000,
        error: 'lightwalletd unavailable',
      },
    });

    expect(status.wallet_sync?.state).toBe('error');
    expect(status.wallet_sync?.fully_scanned_height).toBe(120);
  });

  it('remains compatible with servers that predate wallet status', () => {
    expect(statusSchema.parse(baseStatus).wallet_sync).toBeUndefined();
  });
});

describe('accountSchema', () => {
  const legacyAccount = {
    id: 1,
    name: 'Account 1',
    unified_address: 'account-unified-address',
    transparent_address: 'account-transparent-address',
    transparent_zatoshi: 123,
    orchard_zatoshi: 456,
  };

  it('retains the optional viewing key supplied by the server', () => {
    const account = accountSchema.parse({
      ...legacyAccount,
      unified_full_viewing_key: 'opaque-viewing-key-for-schema-test',
    });
    expect(account.unified_full_viewing_key).toBe('opaque-viewing-key-for-schema-test');
    expect(account.orchard_zatoshi).toBe(456n);
  });

  it('accepts packaged 0.2.1 accounts without a viewing key', () => {
    const account = accountSchema.parse(legacyAccount);
    expect(account.unified_full_viewing_key).toBeUndefined();
    expect(account.transparent_zatoshi).toBe(123n);
  });
});
