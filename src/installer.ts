import type { InstallRequest, InstallReceipt } from './contract.js';
export async function run(_req: InstallRequest): Promise<InstallReceipt> {
  throw new Error('computer_adapter_not_ready: release artifact installation is not enabled in this build');
}
