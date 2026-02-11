export interface DriveInfo {
  id: string;
  name: string;
  instance_url: string;
  sync_path: string;
  icon_path?: string;
  raw_icon_path?: string;
  enabled: boolean;
  user_id: string;
  remote_path: string
  status: DriveStatus;
  capacity?: CapacitySummary;
  linux_mount_mode?: "fuse" | "sync";
  linux_fuse_mounted?: boolean;
  linux_fuse_enabled?: boolean;
}

export type DriveStatus = "active" | "event_push_lost" | "credential_expired";

export interface CapacitySummary {
  total: number;
  used: number;
  label: string;
}
