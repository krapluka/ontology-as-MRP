/// <reference types="vite/client" />

interface ImportMetaEnv {
  /** Optional build-time default for the operator's reviewer identity (the operator can
   *  change it in the header; it is then remembered per browser). */
  readonly VITE_EKG_REVIEWER?: string;
}
