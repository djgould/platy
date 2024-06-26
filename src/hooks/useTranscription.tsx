import { useQuery } from "@tanstack/react-query";
import { invoke } from "@tauri-apps/api/core";

export function useLiveTranscription(isRecording: boolean) {
  return useQuery({
    queryKey: ["get_real_time_transcription"],
    queryFn: async (): Promise<{ full_text: string[] }> => {
      return invoke("get_real_time_transcription");
    },
    refetchInterval: 1000,
    enabled: isRecording,
  });
}

export function useCompleteTranscription(conversationId: number) {
  return useQuery({
    queryKey: ["get_complete_transcription", conversationId],
    queryFn: async (): Promise<{ full_text: string[] }> => {
      return invoke("get_complete_transcription", { conversationId });
    },
    refetchInterval: 1000,
    enabled: !!conversationId,
  });
}
