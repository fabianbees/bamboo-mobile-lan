package com.joelsgc.bamboomobilelan

import java.net.InetAddress
import java.net.Socket
import java.security.SecureRandom
import java.security.cert.X509Certificate
import javax.net.ssl.SSLContext
import javax.net.ssl.SSLSocket
import javax.net.ssl.SSLSocketFactory
import javax.net.ssl.TrustManager
import javax.net.ssl.X509TrustManager

object LanTls {
    /**
     * SSLSocketFactory that accepts any certificate AND disables endpoint
     * identification (hostname verification).  Bambu printers use a self-signed
     * certificate issued to the printer serial number, not the IP address, so
     * standard hostname verification always fails.
     */
    fun socketFactory(): SSLSocketFactory {
        val trustAll = arrayOf<TrustManager>(object : X509TrustManager {
            override fun checkClientTrusted(chain: Array<out X509Certificate>?, authType: String?) {}
            override fun checkServerTrusted(chain: Array<out X509Certificate>?, authType: String?) {}
            override fun getAcceptedIssuers(): Array<X509Certificate> = emptyArray()
        })
        // Use TLSv1.2 explicitly — Bambu printers may not support TLS 1.3.
        val ctx = SSLContext.getInstance("TLSv1.2").also {
            it.init(null, trustAll, SecureRandom())
        }
        val base = ctx.socketFactory

        // Wrap to clear endpointIdentificationAlgorithm on every socket so the
        // JVM's built-in hostname check is also bypassed.
        return object : SSLSocketFactory() {
            override fun getDefaultCipherSuites(): Array<String> = base.defaultCipherSuites
            override fun getSupportedCipherSuites(): Array<String> = base.supportedCipherSuites

            private fun configure(s: Socket): Socket {
                (s as? SSLSocket)?.sslParameters =
                    (s as? SSLSocket)?.sslParameters?.also {
                        it.endpointIdentificationAlgorithm = ""
                    }
                return s
            }

            override fun createSocket(): Socket =
                configure(base.createSocket())

            override fun createSocket(s: Socket, host: String, port: Int, autoClose: Boolean): Socket =
                configure(base.createSocket(s, host, port, autoClose))

            override fun createSocket(host: String, port: Int): Socket =
                configure(base.createSocket(host, port))

            override fun createSocket(host: String, port: Int, localHost: InetAddress, localPort: Int): Socket =
                configure(base.createSocket(host, port, localHost, localPort))

            override fun createSocket(host: InetAddress, port: Int): Socket =
                configure(base.createSocket(host, port))

            override fun createSocket(address: InetAddress, port: Int, localAddress: InetAddress, localPort: Int): Socket =
                configure(base.createSocket(address, port, localAddress, localPort))
        }
    }
}
