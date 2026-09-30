/* M-Spike acceptance fixture (design ADR-018): a program linked against libssl that prints the
 * OpenSSL version of the library loaded at run time and of the headers it was compiled with. */
#include <openssl/crypto.h>
#include <openssl/opensslv.h>
#include <openssl/ssl.h>
#include <stdio.h>

int main(void)
{
	/* A libssl call, so that the program needs libssl.so and not only libcrypto.so. */
	SSL_CTX *ctx = SSL_CTX_new(TLS_client_method());
	if (ctx == NULL)
		return 2;
	printf("runtime: %s\n", OpenSSL_version(OPENSSL_VERSION));
	printf("headers: %s\n", OPENSSL_VERSION_TEXT);
	SSL_CTX_free(ctx);
	/* The major version of the headers and of the loaded library must agree. */
	return (OpenSSL_version_num() >> 28) == (OPENSSL_VERSION_NUMBER >> 28) ? 0 : 1;
}
