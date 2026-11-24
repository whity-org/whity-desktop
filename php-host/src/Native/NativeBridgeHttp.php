<?php

declare(strict_types=1);

namespace Whity\Native;

/**
 * The one HTTP call shape the bridge takes: POST a JSON body to a loopback
 * URL with one auth header, read a JSON answer, turn a refusal into a typed
 * exception. Shared by the per-plugin client and the host's handle request.
 */
final class NativeBridgeHttp
{
    /**
     * @param list<string> $headers
     * @return array<string, mixed>
     * @throws NativeBridgeRefusedException
     * @throws NativeBridgeUnavailableException
     */
    public static function postJson(string $url, array $headers, mixed $payload, int $timeoutSeconds): array
    {
        return self::request('POST', $url, $headers, $payload, $timeoutSeconds);
    }

    /**
     * @param list<string> $headers
     * @return array<string, mixed>
     * @throws NativeBridgeRefusedException
     * @throws NativeBridgeUnavailableException
     */
    public static function getJson(string $url, array $headers, int $timeoutSeconds): array
    {
        return self::request('GET', $url, $headers, null, $timeoutSeconds);
    }

    /**
     * @param list<string> $headers
     * @return array<string, mixed>
     */
    private static function request(string $method, string $url, array $headers, mixed $payload, int $timeoutSeconds): array
    {
        if (!function_exists('curl_init')) {
            throw new NativeBridgeUnavailableException('The curl extension is not loaded; the native bridge needs it.');
        }
        if (!str_starts_with($url, 'http://127.0.0.1:')) {
            throw new NativeBridgeUnavailableException('The native bridge must be on loopback.');
        }
        $body = null;
        if ($method === 'POST') {
            $body = json_encode($payload, JSON_UNESCAPED_SLASHES | JSON_UNESCAPED_UNICODE);
            if ($body === false) {
                throw new NativeBridgeUnavailableException('Failed to encode the request as JSON: ' . json_last_error_msg());
            }
        }

        $curl = curl_init($url);
        if ($curl === false) {
            throw new NativeBridgeUnavailableException('curl_init failed');
        }
        $options = [
            CURLOPT_HTTPHEADER => array_merge(['Content-Type: application/json', 'Accept: application/json'], $headers),
            CURLOPT_RETURNTRANSFER => true,
            CURLOPT_FOLLOWLOCATION => false,
            CURLOPT_PROXY => '',
            CURLOPT_CONNECTTIMEOUT => 2,
            CURLOPT_TIMEOUT => $timeoutSeconds,
            CURLOPT_PROTOCOLS => CURLPROTO_HTTP,
        ];
        if ($body !== null) {
            $options[CURLOPT_POST] = true;
            $options[CURLOPT_POSTFIELDS] = $body;
        }
        curl_setopt_array($curl, $options);
        $raw = curl_exec($curl);
        $status = (int) curl_getinfo($curl, CURLINFO_RESPONSE_CODE);
        $error = curl_error($curl);
        curl_close($curl);

        if (!is_string($raw)) {
            throw new NativeBridgeUnavailableException('The native bridge did not answer: ' . ($error !== '' ? $error : 'no response'));
        }
        /** @var mixed $decoded */
        $decoded = json_decode($raw, true);
        if (!is_array($decoded)) {
            throw new NativeBridgeUnavailableException("The native bridge answered {$status} with no JSON");
        }
        if ($status < 200 || $status >= 300 || ($decoded['ok'] ?? true) === false) {
            $code = is_string($decoded['code'] ?? null) ? $decoded['code'] : 'failed';
            $message = is_string($decoded['error'] ?? null) ? $decoded['error'] : "The native bridge answered {$status}";

            $proposalId = is_string($decoded['proposalId'] ?? null) ? $decoded['proposalId'] : null;

            throw new NativeBridgeRefusedException($code, $message, $status, $proposalId);
        }

        return $decoded;
    }
}
