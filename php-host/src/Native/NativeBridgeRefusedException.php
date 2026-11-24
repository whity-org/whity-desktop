<?php

declare(strict_types=1);

namespace Whity\Native;

/**
 * The bridge was reached and said no. `code` is one of the bridge's codes:
 * `unauthorized`, `not_declared`, `off`, `ask_first`, `unsupported`,
 * `invalid_args`, `failed`. A plugin shows `off` and `ask_first` to the
 * person as a setting to change, not as an error.
 */
final class NativeBridgeRefusedException extends \RuntimeException
{
    public function __construct(
        public readonly string $bridgeCode,
        string $message,
        public readonly int $httpStatus,
    ) {
        parent::__construct($message, $httpStatus);
    }
}
