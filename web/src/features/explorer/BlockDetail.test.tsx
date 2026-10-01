import { describe, expect, it, vi, afterEach } from 'vitest';
import { screen } from '@testing-library/react';
import { MemoryRouter, Route, Routes } from 'react-router-dom';
import { renderWithProviders, requestUrl } from '@/test/utils';
import { BlockDetail } from './BlockDetail';

const IRONWOOD_ROOT = '1f'.repeat(32);
const ORCHARD_ROOT = 'ae'.repeat(32);

afterEach(() => {
  vi.restoreAllMocks();
});

function renderBlock(block: Record<string, unknown>) {
  vi.spyOn(globalThis, 'fetch').mockImplementation((input) => {
    const ok = requestUrl(input).includes('/blocks/106');
    return Promise.resolve(
      new Response(JSON.stringify(ok ? block : { error: { message: 'unexpected', status: 404 } }), {
        status: ok ? 200 : 404,
        headers: { 'content-type': 'application/json' },
      }),
    );
  });

  renderWithProviders(
    <MemoryRouter initialEntries={['/explorer/block/106']}>
      <Routes>
        <Route path="/explorer/block/:id" element={<BlockDetail />} />
      </Routes>
    </MemoryRouter>,
  );
}

const block = {
  hash: 'bc'.repeat(32),
  height: 106,
  time: 1_296_688_602,
  size: 9_500,
  nTx: 2,
  confirmations: 1,
  tx: [],
  finalorchardroot: ORCHARD_ROOT,
};

describe('BlockDetail', () => {
  it('shows the Ironwood root the server adds from z_gettreestate', async () => {
    renderBlock({ ...block, finalironwoodroot: IRONWOOD_ROOT });

    expect(await screen.findByText('Ironwood root')).toBeInTheDocument();
    expect(screen.getByText(IRONWOOD_ROOT)).toBeInTheDocument();
    expect(screen.getByText(ORCHARD_ROOT)).toBeInTheDocument();
  });

  it('leaves the row out before NU6.3', async () => {
    renderBlock(block);

    expect(await screen.findByText('Orchard root')).toBeInTheDocument();
    expect(screen.queryByText('Ironwood root')).not.toBeInTheDocument();
  });
});
