<?php

declare(strict_types=1);

namespace BridgeProbe;

use Whity\Native\NativeBridgeClients;
use Whity\Native\NativeBridgeRefusedException;
use Whity\Sdk\Http\Request;
use Whity\Sdk\Http\Response;
use Whity\Sdk\PluginInterface;

/**
 * A plugin for the end-to-end test of the native bridge: it declares
 * `printer.list` and nothing else, lists the printers through the bridge,
 * and shows what the bridge answers when it asks for what it never declared.
 * Lives in the drivers crate's test fixtures; the host loads it from
 * WHITY_DOWNLOADED_PLUGINS_ROOT in the smoke test only.
 */
final class BridgeProbePlugin implements PluginInterface
{
    public function getName(): string
    {
        return 'BridgeProbe';
    }

    public function getVersion(): string
    {
        return '0.1.0';
    }

    /** @return list<string> */
    public function getDesktopCapabilities(): array
    {
        return ['printer.list'];
    }

    public function getRoutes(): array
    {
        return [
            [
                'method' => 'GET',
                'path' => '/api/bridge-probe/printers',
                'handler' => fn (Request $request, array $params): Response => $this->answer('printer.list', []),
            ],
            [
                'method' => 'POST',
                'path' => '/api/bridge-probe/print',
                'handler' => fn (Request $request, array $params): Response => $this->answer('printer.print', ['text' => 'probe']),
            ],
        ];
    }

    public function getPermissions(): array
    {
        return [];
    }

    public function getHooks(): array
    {
        return [];
    }

    public function getMigrations(): array
    {
        return [];
    }

    /** @param array<string, mixed> $args */
    private function answer(string $capability, array $args): Response
    {
        try {
            $result = \Whity\app(NativeBridgeClients::class)->for($this->getName())->call($capability, $args);

            return Response::json(['ok' => true, 'result' => $result]);
        } catch (NativeBridgeRefusedException $e) {
            return Response::json(['ok' => false, 'code' => $e->bridgeCode, 'error' => $e->getMessage()], $e->httpStatus);
        }
    }
}
