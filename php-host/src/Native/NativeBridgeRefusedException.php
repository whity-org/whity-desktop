<?php

declare(strict_types=1);

namespace Whity\Native;

/**
 * The bridge was reached and did not run the call. `bridgeCode` is one of
 * the bridge's codes: `unauthorized`, `not_declared`, `off`, `ask_first`,
 * `unsupported`, `invalid_args`, `failed`. `off` is a setting the person can
 * change, not an error. `ask_first` is not a failure at all: the call became
 * a proposal, `proposalId` names it, and
 * {@see NativeBridgeClient::proposal()} tells whether it ran.
 */
final class NativeBridgeRefusedException extends \RuntimeException
{
    public function __construct(
        public readonly string $bridgeCode,
        string $message,
        public readonly int $httpStatus,
        public readonly ?string $proposalId = null,
    ) {
        parent::__construct($message, $httpStatus);
    }
}
