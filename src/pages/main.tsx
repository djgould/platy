"use client";
import {
  useAudioInputDevicesQuery,
  useAudioOutputDevicesQuery,
} from "@/hooks/useMediaDevices";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { useAtom } from "jotai";
import {
  selectedAudioInputDeviceAtom,
  selectedAudioOutputDeviceAtom,
} from "@/atoms/audioDeviceAtom";
import { useEffect, useState } from "react";
import {
  useStartRecorderMutation,
  useStopRecorderMutation,
} from "@/hooks/useRecorder";
import { Circle, Eye, Loader, Trash } from "lucide-react";
import { Button } from "@/components/ui/button";
import clsx from "clsx";
import {
  Card,
  CardContent,
  CardFooter,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Separator } from "@/components/ui/separator";
import {
  useConversations,
  useCreateConversationMutation,
  useDeleteConversationMutation,
} from "@/hooks/useConversations";
import {
  Table,
  TableBody,
  TableCaption,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import Link from "next/link";
import { invoke } from "@tauri-apps/api/core";
import NavBar from "@/components/nav/NavBar";

export default function Page() {
  const audioInputDevices = useAudioInputDevicesQuery();
  const audioOutputDevices = useAudioOutputDevicesQuery();

  const [selectedAudioInputDevice, setSelectedAudioInputDevice] = useAtom(
    selectedAudioInputDeviceAtom
  );
  const [selectedAudioOutputDevice, setSelectedAudioOutputDevice] = useAtom(
    selectedAudioOutputDeviceAtom
  );
  const [activeRecordingInfo, setActiveRecordingInfo] = useState<
    { conversation_id: number; status: "recording" | "stopping" } | undefined
  >();

  const startRecorderMutation = useStartRecorderMutation();
  const stopRecorderMutation = useStopRecorderMutation();

  const conversations = useConversations();
  const createConversationMutation = useCreateConversationMutation();
  const deleteConversationMutation = useDeleteConversationMutation();

  const startRecording = async () => {
    createConversationMutation.mutate(undefined, {
      onSuccess(conversation) {
        setActiveRecordingInfo({
          conversation_id: conversation.lastInsertId,
          status: "recording",
        });
        startRecorderMutation.mutate(
          {
            conversation_id: conversation.lastInsertId,
          },
          {
            onError: () => {
              setActiveRecordingInfo(undefined);
            },
          }
        );
      },
    });
  };

  const stopRecording = () => {
    if (!activeRecordingInfo?.conversation_id) return;
    setActiveRecordingInfo({
      ...activeRecordingInfo,
      status: "stopping",
    });
    stopRecorderMutation.mutate(
      { conversation_id: activeRecordingInfo?.conversation_id },
      {
        onSuccess: () => {
          setActiveRecordingInfo(undefined);
        },
      }
    );
  };

  return (
    <>
      <NavBar />
      <div className="flex flex-col sm:gap-4 sm:py-4 sm:pl-14">
        <div className="p-2 h-screen flex flex-col gap-4">
          <Card>
            <CardHeader>
              <CardTitle className="text-lg">Your Converstations</CardTitle>
            </CardHeader>
            <CardContent className="flex-1 overflow-y-scroll">
              <Table>
                <TableCaption>
                  A list of your recent conversations.
                </TableCaption>
                <TableHeader>
                  <TableRow>
                    <TableHead className="w-[100px]">Created at</TableHead>
                    <TableHead className="w-[100px]">Actions</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {conversations.data?.map((conversation) => (
                    <TableRow key={conversation.id}>
                      <TableCell className="font-medium">
                        {new Date(conversation.created_at).toLocaleDateString()}
                      </TableCell>
                      <TableCell className="font-medium flex justify-between">
                        <Link href={`/conversations/${conversation.id}`}>
                          <Button size={"sm"} variant={"secondary"}>
                            <Eye />
                          </Button>
                        </Link>

                        <Button
                          size={"sm"}
                          variant={"secondary"}
                          onClick={() => {
                            deleteConversationMutation.mutate({
                              conversationId: conversation.id,
                            });
                          }}
                        >
                          <Trash />
                        </Button>
                      </TableCell>
                    </TableRow>
                  ))}
                </TableBody>
              </Table>
            </CardContent>
            <CardFooter className="flex flex-col gap-4"></CardFooter>
          </Card>
        </div>
      </div>
    </>
  );
}