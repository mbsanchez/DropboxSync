import { Component, type ReactNode } from "react";

type ErrorBoundaryProps = {
  children: ReactNode;
};

type ErrorBoundaryState = {
  hasError: boolean;
  error?: Error;
};

class ErrorBoundary extends Component<ErrorBoundaryProps, ErrorBoundaryState> {
  state: ErrorBoundaryState = { hasError: false, error: undefined };

  static getDerivedStateFromError(error: Error): ErrorBoundaryState {
    return { hasError: true, error };
  }

  componentDidCatch(error: Error, info: React.ErrorInfo) {
    console.error("Uncaught UI error:", error, info);
  }

  private handleTryAgain = () => {
    this.setState({ hasError: false, error: undefined });
  };

  private handleReload = () => {
    window.location.reload();
  };

  render() {
    if (this.state.hasError) {
      const message = this.state.error?.message ?? String(this.state.error);
      return (
        <div
          style={{
            display: "flex",
            justifyContent: "center",
            alignItems: "center",
            minHeight: "100vh",
            padding: 24,
            boxSizing: "border-box",
          }}
        >
          <div
            style={{
              maxWidth: 480,
              width: "100%",
              padding: 24,
              borderRadius: 8,
              border: "1px solid rgba(128, 128, 128, 0.35)",
              background: "rgba(128, 128, 128, 0.08)",
            }}
          >
            <h2 style={{ marginTop: 0 }}>Something went wrong</h2>
            <p style={{ opacity: 0.75 }}>
              The app hit an unexpected error. You can try again, or reload the app if the
              problem persists.
            </p>
            <pre
              style={{
                whiteSpace: "pre-wrap",
                wordBreak: "break-word",
                padding: 12,
                borderRadius: 6,
                background: "rgba(128, 128, 128, 0.15)",
                fontSize: "0.85em",
                opacity: 0.85,
              }}
            >
              {message}
            </pre>
            <div style={{ display: "flex", gap: 8, marginTop: 16 }}>
              <button type="button" onClick={this.handleTryAgain}>
                Try again
              </button>
              <button type="button" onClick={this.handleReload}>
                Reload
              </button>
            </div>
          </div>
        </div>
      );
    }

    return this.props.children;
  }
}

export default ErrorBoundary;
