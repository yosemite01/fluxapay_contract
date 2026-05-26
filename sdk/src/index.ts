import {
  Client as ContractClient,
  Merchant,
  PaymentCharge,
  Refund,
  Dispute,
  PaymentStatus,
  RefundStatus,
  DisputeStatus,
} from "./contracts/fluxapay/src/index.js";
import { Networks } from "@stellar/stellar-sdk";
import {
  FluxapayOfflineSigner,
  OfflineTransactionPayload,
  buildOfflinePayload,
  buildCreatePaymentPayload,
  buildVerifyPaymentPayload,
  buildCreateRefundPayload,
  prepareForOfflineSigning,
  restoreFromOfflinePayload,
} from "./offline-signer.js";

export interface FluxapayConfig {
  network: "testnet" | "mainnet";
  rpcUrl: string;
  contractId: string;
}

export class FluxapayClient {
  public contract: ContractClient;

  constructor(config: FluxapayConfig) {
    this.contract = new ContractClient({
      networkPassphrase:
        config.network === "mainnet" ? Networks.PUBLIC : Networks.TESTNET,
      rpcUrl: config.rpcUrl,
      contractId: config.contractId,
    });
  }

  /**
   * Create a new payment charge
   */
  async createPayment(params: {
    paymentId: string;
    merchantId: string;
    amount: bigint;
    currency: string;
    depositAddress: string;
    expiresAt: bigint;
  }) {
    return this.contract.create_payment({
      payment_id: params.paymentId,
      merchant_id: params.merchantId,
      amount: params.amount,
      currency: params.currency,
      deposit_address: params.depositAddress,
      expires_at: params.expiresAt,
    });
  }

  /**
   * Verify a payment via oracle
   */
  async verifyPayment(params: {
    oracle: string;
    paymentId: string;
    transactionHash: Buffer;
    payerAddress: string;
    amountReceived: bigint;
  }) {
    return this.contract.verify_payment({
      oracle: params.oracle,
      payment_id: params.paymentId,
      transaction_hash: params.transactionHash,
      payer_address: params.payerAddress,
      amount_received: params.amountReceived,
    });
  }

  /**
   * Create a refund request
   */
  async createRefund(params: {
    paymentId: string;
    amount: bigint;
    reason: string;
    requester: string;
  }) {
    return this.contract.create_refund({
      payment_id: params.paymentId,
      refund_amount: params.amount,
      reason: params.reason,
      requester: params.requester,
    });
  }

  /**
   * Get merchant details
   */
  async getMerchant(merchantId: string) {
    return this.contract.get_merchant({
      merchant_id: merchantId,
    });
  }

  /**
   * Get payment details
   */
  async getPayment(paymentId: string) {
    return this.contract.get_payment({ payment_id: paymentId });
  }

  /** Offline/hardware wallet payload builder utilities. */
  offlineSigner(): FluxapayOfflineSigner {
    return new FluxapayOfflineSigner(
      this.contract as import("./offline-signer.js").OfflineCapableClient,
      this.contract.options.contractId,
      this.contract.options.networkPassphrase,
    );
  }
}

export {
  Merchant,
  PaymentCharge,
  Refund,
  Dispute,
  PaymentStatus,
  RefundStatus,
  DisputeStatus,
  FluxapayOfflineSigner,
  OfflineTransactionPayload,
  buildOfflinePayload,
  buildCreatePaymentPayload,
  buildVerifyPaymentPayload,
  buildCreateRefundPayload,
  prepareForOfflineSigning,
  restoreFromOfflinePayload,
};
