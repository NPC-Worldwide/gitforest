#!/usr/bin/env python3
import os
import sys

from eth_account import Account
from eth_utils import to_checksum_address


def main() -> int:
    try:
        key = os.environ["GF_EVM_SIGNER_KEY"]
        tx = {
            "to": to_checksum_address(os.environ["TX_TO"]),
            "value": int(os.environ["TX_VALUE_WEI"]),
            "nonce": int(os.environ["TX_NONCE"]),
            "gasPrice": int(os.environ["TX_GAS_PRICE_WEI"]),
            "gas": int(os.environ["TX_GAS_LIMIT"]),
            "chainId": int(os.environ["TX_CHAIN_ID"]),
        }
    except KeyError as e:
        print(f"missing env: {e}", file=sys.stderr)
        return 1
    signed = Account.sign_transaction(tx, key)
    print("0x" + signed.raw_transaction.hex())
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
