import { AssembledTransaction } from "@stellar/stellar-sdk/contract";
import { Client as ContractClient } from "./contracts/fluxapay/src/index.js";

/** Serializable payload for hardware/offline signing workflows (Issue #232). */
export interface OfflineTransactionPayload {
  /** Soroban contract method name. */
  method: string;
  /** Target contract ID (C...). */
  contractId: string;
  /** Network passphrase used to build the transaction. */
  networkPassphrase: string;
  /** Base64 XDR of the unsigned transaction envelope. */
  unsignedXdr: string;
  /** Hex-encoded transaction hash for wallet display/verification. */
  hash: string;
  /** JSON snapshot compatible with `Client.fromJSON.<method>()`. */
  json: string;
  /** Addresses that must sign auth entries before submission. */
  requiredAuthSigners: string[];
}

export type OfflineCapableClient = ContractClient & {
  fromJSON: Record<string, (json: string) => AssembledTransaction<unknown>>;
};

/** Ensure simulation completed so payload fields are populated. */
export async function prepareForOfflineSigning<T>(
  tx: AssembledTransaction<T>,
): Promise<AssembledTransaction<T>> {
  if (!tx.simulation) {
    await tx.simulate();
  }
  return tx;
}

/** Build a raw offline payload from a simulated assembled transaction. */
export async function buildOfflinePayload<T>(
  method: string,
  contractId: string,
  networkPassphrase: string,
  tx: AssembledTransaction<T>,
): Promise<OfflineTransactionPayload> {
  const prepared = await prepareForOfflineSigning(tx);

  return {
    method,
    contractId,
    networkPassphrase,
    unsignedXdr: prepared.toXDR(),
    hash: prepared.built?.hash().toString("hex") ?? "",
    json: prepared.toJSON(),
    requiredAuthSigners: prepared.needsNonInvokerSigningBy(),
  };
}

/** Restore an assembled transaction from a previously exported JSON payload. */
export function restoreFromOfflinePayload(
  client: OfflineCapableClient,
  payload: Pick<OfflineTransactionPayload, "method" | "json">,
): AssembledTransaction<unknown> {
  const restore = client.fromJSON[payload.method];
  if (!restore) {
    throw new Error(`Unknown contract method for offline restore: ${payload.method}`);
  }
  return restore(payload.json);
}

/** Raw payload builder for `create_payment` invocations. */
export async function buildCreatePaymentPayload(
  client: OfflineCapableClient,
  contractId: string,
  networkPassphrase: string,
  args: Parameters<ContractClient["create_payment"]>[0],
): Promise<OfflineTransactionPayload> {
  const tx = await client.create_payment(args);
  return buildOfflinePayload("create_payment", contractId, networkPassphrase, tx);
}

/** Raw payload builder for `verify_payment` invocations. */
export async function buildVerifyPaymentPayload(
  client: OfflineCapableClient,
  contractId: string,
  networkPassphrase: string,
  args: Parameters<ContractClient["verify_payment"]>[0],
): Promise<OfflineTransactionPayload> {
  const tx = await client.verify_payment(args);
  return buildOfflinePayload("verify_payment", contractId, networkPassphrase, tx);
}

/** Raw payload builder for `create_refund` invocations. */
export async function buildCreateRefundPayload(
  client: OfflineCapableClient,
  contractId: string,
  networkPassphrase: string,
  args: Parameters<ContractClient["create_refund"]>[0],
): Promise<OfflineTransactionPayload> {
  const tx = await client.create_refund(args);
  return buildOfflinePayload("create_refund", contractId, networkPassphrase, tx);
}

/**
 * High-level helper exposing common raw payload builders for hardware wallets.
 */
export class FluxapayOfflineSigner {
  constructor(
    private readonly client: OfflineCapableClient,
    private readonly contractId: string,
    private readonly networkPassphrase: string,
  ) {}

  buildCreatePayment(
    args: Parameters<ContractClient["create_payment"]>[0],
  ): Promise<OfflineTransactionPayload> {
    return buildCreatePaymentPayload(
      this.client,
      this.contractId,
      this.networkPassphrase,
      args,
    );
  }

  buildVerifyPayment(
    args: Parameters<ContractClient["verify_payment"]>[0],
  ): Promise<OfflineTransactionPayload> {
    return buildVerifyPaymentPayload(
      this.client,
      this.contractId,
      this.networkPassphrase,
      args,
    );
  }

  buildCreateRefund(
    args: Parameters<ContractClient["create_refund"]>[0],
  ): Promise<OfflineTransactionPayload> {
    return buildCreateRefundPayload(
      this.client,
      this.contractId,
      this.networkPassphrase,
      args,
    );
  }

  restore(payload: OfflineTransactionPayload): AssembledTransaction<unknown> {
    return restoreFromOfflinePayload(this.client, payload);
  }
}
